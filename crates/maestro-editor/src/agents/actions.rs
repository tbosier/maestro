//! What the user can do with an agent: run its tests, send feedback,
//! answer permission requests, reject, integrate and archive.
//!
//! Consequential actions go through a confirmation dialog that says what
//! will happen. The dialog is not the enforcement: `maestro-git` checks
//! again (a dirty worktree, conflicts, work that removal would lose) and
//! refuses on its own.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use maestro_git::{Head, IntegrateError, Integration, RemoveError, RemoveRefusal};
use maestro_tasks::TaskEvent;

use super::panel::{ChoicePurpose, Confirm, ConfirmAction, DetailView, InputPurpose, Overlay};
use super::{AgentId, ProviderKind, Runtime, Status, TestFinish, TestRun};
use crate::editor::{Editor, normalize_path};

/// Lists at most five paths, then how many more.
fn list_paths(paths: &[PathBuf]) -> String {
    let mut s: Vec<String> = paths
        .iter()
        .take(5)
        .map(|p| p.display().to_string())
        .collect();
    if paths.len() > 5 {
        s.push(format!("and {} more", paths.len() - 5));
    }
    s.join(", ")
}

/// Modified buffers whose files are under `root`.
fn unsaved_under(ed: &Editor, root: &Path) -> Vec<String> {
    let root = normalize_path(root);
    ed.buffers
        .values()
        .filter(|b| b.doc.is_modified())
        .filter(|b| {
            b.doc
                .path()
                .is_some_and(|p| normalize_path(p).starts_with(&root))
        })
        .map(|b| b.doc.display_name())
        .collect()
}

// ----- verification ------------------------------------------------------

/// The verification command for a worktree: `[agents] test-command`, or
/// `cargo test` when the worktree has a `Cargo.toml`.
pub fn test_command(ed: &Editor, root: &Path) -> Option<Vec<String>> {
    let configured = &ed.config.agents.test_command;
    if !configured.is_empty() {
        return Some(configured.clone());
    }
    root.join("Cargo.toml")
        .is_file()
        .then(|| vec!["cargo".to_string(), "test".to_string()])
}

/// `t`: runs the verification command in the agent's worktree.
pub fn run_tests(ed: &mut Editor, id: AgentId) {
    let tx = ed.idle_job_sender();
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    let Some(root) = a.worktree.as_ref().map(|w| w.path.clone()) else {
        ed.error("the agent has no worktree yet");
        return;
    };
    if a.archived {
        ed.error("the agent's worktree was archived");
        return;
    }
    if a.tests_running() {
        ed.error("tests are already running for this agent");
        return;
    }
    let Some(cmd) = test_command(ed, &root) else {
        ed.error("no test command: the worktree has no Cargo.toml; set [agents] test-command");
        return;
    };
    let spec = maestro_tasks::TaskSpec {
        program: cmd[0].clone(),
        args: cmd[1..].to_vec(),
        cwd: root,
    };
    let shown = cmd.join(" ");
    // Lines collect here and one job drains them, so a chatty test run
    // cannot queue unbounded jobs: the buffer keeps at most
    // MAX_TEST_LINES lines and counts the rest as dropped.
    let buffer: Arc<Mutex<OutputBuffer>> = Default::default();
    let started = maestro_tasks::start(&spec, move |event| match event {
        TaskEvent::Output(line) => {
            let mut b = buffer.lock().unwrap_or_else(|e| e.into_inner());
            b.lines.push_back(line);
            if b.lines.len() > super::MAX_TEST_LINES {
                b.lines.pop_front();
                b.dropped += 1;
            }
            if !b.scheduled {
                b.scheduled = true;
                let buffer = buffer.clone();
                tx.send(move |ed| drain_output(ed, id, &buffer));
            }
        }
        finished => {
            let buffer = buffer.clone();
            tx.send(move |ed| {
                drain_output(ed, id, &buffer);
                on_test_event(ed, id, finished);
            });
        }
    });
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    match started {
        Ok(handle) => {
            a.tests.push(TestRun {
                command: shown.clone(),
                started: Instant::now(),
                output: VecDeque::new(),
                dropped: 0,
                finished: None,
            });
            a.test_handle = Some(handle);
            a.log(format!("tests started: {shown}"));
            ed.agents.panel.view = DetailView::Tests;
            ed.agents.panel.scroll = 0;
            ed.info(format!("running {shown}"));
        }
        Err(e) => {
            let msg = format!("could not run {shown}: {e}");
            a.log(msg.clone());
            ed.error(msg);
        }
    }
}

/// Test output waiting for the editor thread.
#[derive(Default)]
struct OutputBuffer {
    lines: VecDeque<String>,
    dropped: usize,
    /// A job to drain the buffer is queued.
    scheduled: bool,
}

fn drain_output(ed: &mut Editor, id: AgentId, buffer: &Mutex<OutputBuffer>) {
    let (lines, dropped) = {
        let mut b = buffer.lock().unwrap_or_else(|e| e.into_inner());
        b.scheduled = false;
        (std::mem::take(&mut b.lines), std::mem::take(&mut b.dropped))
    };
    let Some(run) = ed.agents.get_mut(id).and_then(|a| a.tests.last_mut()) else {
        return;
    };
    run.dropped += dropped;
    for line in lines {
        on_line(run, line);
    }
}

fn on_line(run: &mut TestRun, line: String) {
    run.output.push_back(line);
    if run.output.len() > super::MAX_TEST_LINES {
        run.output.pop_front();
        run.dropped += 1;
    }
}

fn on_test_event(ed: &mut Editor, id: AgentId, event: TaskEvent) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    let Some(run) = a.tests.last_mut() else {
        return;
    };
    match event {
        TaskEvent::Output(line) => on_line(run, line),
        TaskEvent::Finished { code, duration } => {
            let text: Vec<&str> = run.output.iter().map(String::as_str).collect();
            let summary = maestro_tasks::parse_cargo_test(&text.join("\n"));
            run.finished = Some(TestFinish {
                code,
                duration,
                summary,
            });
            let desc = run.describe();
            a.test_handle = None;
            a.log(format!("tests {desc}"));
        }
    }
}

// ----- feedback ----------------------------------------------------------

/// Why feedback cannot reach the agent now, if it cannot.
fn feedback_blocker(a: &super::Agent) -> Option<String> {
    match &a.runtime {
        Runtime::None => Some("the agent is not running; feedback cannot reach it".into()),
        Runtime::Acp(_) if a.exited.is_some() => {
            Some("the agent process has exited; feedback cannot reach it".into())
        }
        Runtime::Acp(_) if a.turn_running => {
            Some("the agent is still working on a turn; send feedback when it ends".into())
        }
        Runtime::Terminal(t) if t.exit_status().is_some() => {
            Some("the agent's program has exited; feedback cannot reach it".into())
        }
        _ => None,
    }
}

/// `f`: asks for feedback text to send with the review comments.
pub fn ask_feedback(ed: &mut Editor, id: AgentId) {
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    if let Some(why) = feedback_blocker(a) {
        ed.error(why);
        return;
    }
    let comments = a.comments.iter().filter(|c| !c.sent).count();
    let label = match a.kind() {
        ProviderKind::Acp => format!("feedback (a new prompt, with {comments} comment(s))"),
        ProviderKind::Terminal => format!(
            "feedback (typed into the terminal with {comments} comment(s); that is all Maestro can do)"
        ),
    };
    ed.agents.panel.overlay = Some(Overlay::Input {
        purpose: InputPurpose::Feedback(id),
        label,
        text: String::new(),
    });
}

/// The message feedback sends: unsent comments, then the typed text.
pub fn feedback_message(a: &super::Agent, text: &str) -> String {
    let mut msg = String::new();
    let comments: Vec<&super::Comment> = a.comments.iter().filter(|c| !c.sent).collect();
    if !comments.is_empty() {
        msg.push_str("Review comments:\n");
        for c in comments {
            let side = if c.old { " (removed line)" } else { "" };
            msg.push_str(&format!(
                "- {}:{}{side}: {}\n",
                c.path.display(),
                c.line,
                c.text
            ));
        }
        if !text.is_empty() {
            msg.push('\n');
        }
    }
    msg.push_str(text);
    msg.trim_end().to_string()
}

pub fn send_feedback(ed: &mut Editor, id: AgentId, text: &str) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    if let Some(why) = feedback_blocker(a) {
        ed.error(why);
        return;
    }
    let msg = feedback_message(a, text);
    if msg.is_empty() {
        ed.error("nothing to send: no comments and no text");
        return;
    }
    let result = match &a.runtime {
        Runtime::Acp(c) => c.prompt(&msg).map(|()| {
            a.turn_running = true;
            a.transcript
                .push(maestro_agent::TranscriptEntry::User(msg.clone()));
            a.set_status(Status::Working);
            "sent the feedback as a new prompt".to_string()
        }),
        Runtime::Terminal(t) => {
            let mut bytes = maestro_term::encode_paste(&msg, t.bracketed_paste());
            bytes.push(b'\r');
            t.write(&bytes).map(|()| {
                "typed the feedback into the terminal; Maestro cannot tell whether the program reads it"
                    .to_string()
            })
        }
        Runtime::None => Err(std::io::Error::other("not running")),
    };
    match result {
        Ok(done) => {
            for c in &mut a.comments {
                c.sent = true;
            }
            a.log(done.clone());
            ed.info(done);
        }
        Err(e) => {
            let msg = format!("feedback was not sent: {e}");
            a.log(msg.clone());
            ed.error(msg);
        }
    }
}

// ----- permissions ---------------------------------------------------------

/// `p`: lists the choices of the pending permission request.
pub fn choose_permission(ed: &mut Editor, id: AgentId) {
    let Some(req) = ed
        .agents
        .get(id)
        .and_then(|a| a.permissions.first().cloned())
    else {
        ed.error("no permission request is pending for this agent");
        return;
    };
    let items = req
        .options
        .iter()
        .map(|o| (o.label.clone(), o.kind.replace('_', " ")))
        .collect();
    ed.agents.panel.overlay = Some(Overlay::Choice {
        purpose: ChoicePurpose::Permission(id),
        title: format!("permission: {}", req.tool_title),
        items,
        selected: 0,
    });
}

/// A choice was picked. Granting asks first; refusing is sent at once.
pub fn permission_chosen(ed: &mut Editor, id: AgentId, index: usize) {
    let Some(req) = ed
        .agents
        .get(id)
        .and_then(|a| a.permissions.first().cloned())
    else {
        ed.error("the permission request is no longer pending");
        return;
    };
    let Some(choice) = req.options.get(index).cloned() else {
        return;
    };
    if choice.kind.starts_with("allow") {
        let mut lines = vec![
            format!("Lets the agent run: {}", req.tool_title),
            format!(
                "Answer: {} ({})",
                choice.label,
                choice.kind.replace('_', " ")
            ),
        ];
        if choice.kind == "allow_always" {
            lines.push("The agent may then do this again without asking.".into());
        }
        lines.push("The worktree isolates files, not privileges: the agent runs as you.".into());
        ed.agents.panel.overlay = Some(Overlay::Confirm(Confirm {
            title: "grant permission".into(),
            lines,
            keys: vec![("y".into(), "grant".into()), ("n".into(), "cancel".into())],
            action: ConfirmAction::Grant {
                agent: id,
                request_id: req.request_id,
                option: choice.id,
            },
        }));
    } else {
        answer_permission(ed, id, req.request_id, &choice.id);
    }
}

pub fn answer_permission(
    ed: &mut Editor,
    id: AgentId,
    request_id: serde_json::Value,
    option: &str,
) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    let label = a
        .permissions
        .iter()
        .find(|p| p.request_id == request_id)
        .and_then(|p| p.options.iter().find(|o| o.id == option))
        .map(|o| o.label.clone())
        .unwrap_or_else(|| option.to_string());
    let Some(client) = a.runtime.acp() else {
        ed.error("the agent is not running");
        return;
    };
    match client.answer_permission(request_id.clone(), Some(option)) {
        Ok(()) => {
            a.permissions.retain(|p| p.request_id != request_id);
            // Still awaiting approval while other requests wait.
            if a.turn_running && a.permissions.is_empty() {
                a.set_status(Status::Working);
            }
            a.log(format!("permission answered: {label}"));
            ed.info(format!("answered: {label}"));
        }
        Err(e) => ed.error(format!("the answer was not sent: {e}")),
    }
}

// ----- reject and archive --------------------------------------------------

/// Why an action cannot run while Git work for the agent is in flight.
fn busy(a: &super::Agent) -> Option<String> {
    a.git_busy
        .map(|what| format!("{} is busy ({what}); wait for it to finish", a.name))
}

/// `X`: asks before rejecting.
pub fn confirm_reject(ed: &mut Editor, id: AgentId) {
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    if a.base_status() == Status::Rejected {
        ed.error("the agent is already rejected; A archives its worktree");
        return;
    }
    if a.base_status() == Status::Integrated {
        ed.error("the agent's work is already integrated; rejecting it now changes nothing");
        return;
    }
    // Rejecting while the worktree is still being made is allowed: the
    // agent is then never launched.
    if let Some(why) = busy(a).filter(|_| a.git_busy != Some(super::CREATING)) {
        ed.error(why);
        return;
    }
    let mut lines = vec![];
    if a.is_running() || a.tests_running() {
        lines.push(format!(
            "Stops {} and its tests, and marks it rejected.",
            a.name
        ));
    } else {
        lines.push(format!("Marks {} rejected.", a.name));
    }
    match &a.worktree {
        Some(w) => {
            lines.push(format!(
                "y keeps its worktree {} and branch {}.",
                w.path.display(),
                w.branch
            ));
            lines.push(
                "a also removes the worktree. Ignored files in it (build output, .env) are deleted without asking; if it holds uncommitted or untracked files, you are asked again first."
                    .into(),
            );
        }
        None => lines.push("Its worktree is still being made; the agent will not start.".into()),
    }
    lines.push(
        "Maestro copies nothing into the main worktree. It cannot undo what the agent's own tools wrote outside its worktree."
            .into(),
    );
    ed.agents.panel.overlay = Some(Overlay::Confirm(Confirm {
        title: format!("reject {}", a.name),
        lines,
        keys: vec![
            ("y".into(), "reject".into()),
            ("a".into(), "reject and archive".into()),
            ("n".into(), "cancel".into()),
        ],
        action: ConfirmAction::Reject(id),
    }));
}

/// Asks before discarding the agent's change to one file (`D` in the
/// review).
pub fn ask_drop_file(ed: &mut Editor, id: AgentId, file: &maestro_git::DiffFile) {
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    let Some(w) = a.worktree.clone() else {
        return;
    };
    let base = &w.base[..w.base.len().min(8)];
    let shown = |p: &Option<PathBuf>| p.as_deref().map(|p| p.display().to_string());
    let (old, new) = (file.old_path.clone(), file.new_path.clone());
    let what = match (shown(&old), shown(&new)) {
        (Some(o), Some(n)) if o == n => {
            format!("{n} goes back to its version at {base}, the commit the agent started from.")
        }
        (Some(o), Some(n)) => {
            format!("{n} is deleted and {o} comes back as it was at {base}.")
        }
        (None, Some(n)) => format!("{n} is deleted; the agent added it."),
        (Some(o), None) => format!("{o} comes back as it was at {base}; the agent deleted it."),
        (None, None) => return,
    };
    let mut lines = vec![
        format!(
            "Discard {}'s change to this file in {}.",
            a.name,
            w.path.display()
        ),
        what,
        "The agent's version is not kept anywhere if it was never committed; this cannot be undone from Maestro."
            .into(),
    ];
    if a.is_running() {
        lines.push("The agent is still running and may change the file again.".into());
    }
    ed.agents.panel.overlay = Some(Overlay::Confirm(Confirm {
        title: format!("drop a change from {}", a.name),
        lines,
        keys: vec![
            ("y".into(), "discard the change".into()),
            ("n".into(), "cancel".into()),
        ],
        action: ConfirmAction::DropFile {
            agent: id,
            old,
            new,
        },
    }));
}

/// Discards the agent's change to one file in its worktree, on a
/// background thread, then refreshes the review.
pub fn drop_file(ed: &mut Editor, id: AgentId, old: Option<PathBuf>, new: Option<PathBuf>) {
    let git = ed.git.git().clone();
    let Some(w) = ed.agents.get(id).and_then(|a| a.worktree.clone()) else {
        return;
    };
    let touched: Vec<PathBuf> = [&old, &new]
        .into_iter()
        .flatten()
        .map(|p| w.path.join(p))
        .collect();
    let unsaved: Vec<String> = touched.iter().flat_map(|p| unsaved_under(ed, p)).collect();
    if !unsaved.is_empty() {
        ed.error(format!(
            "not dropped: {} has unsaved edits; save or close it first",
            unsaved.join(", ")
        ));
        return;
    }
    let label = new
        .as_deref()
        .or(old.as_deref())
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let spawned = ed.spawn_background("agent-drop", move |tx| {
        let result = open_exact(&git, &w.path).and_then(|repo| {
            repo.drop_change(&w.base, old.as_deref(), new.as_deref())
                .map_err(|e| e.to_string())
        });
        tx.send(move |ed| {
            match result {
                Ok(()) => {
                    if let Some(a) = ed.agents.get_mut(id) {
                        a.log(format!("dropped the change to {label}"));
                    }
                    ed.info(format!("dropped the change to {label}"));
                }
                Err(e) => ed.error(format!("could not drop the change to {label}: {e}")),
            }
            super::review::request(ed, id);
        });
    });
    if let Err(e) = spawned {
        ed.error(format!("could not start: {e}"));
    }
}

pub fn reject(ed: &mut Editor, id: AgentId, then_archive: bool) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    a.stop();
    a.decide(Status::Rejected);
    a.permissions.clear();
    a.log("rejected by the user");
    let name = a.name.clone();
    let has_worktree = a.worktree.is_some();
    if ed.agents.panel.terminal_focus == Some(id) {
        ed.agents.panel.terminal_focus = None;
    }
    ed.info(format!("rejected {name}"));
    if then_archive {
        if has_worktree {
            archive(ed, id, false);
        } else {
            ed.info(format!(
                "rejected {name}; its worktree is still being made, archive it with A once it exists"
            ));
        }
    }
}

/// `A`: asks before removing the worktree.
pub fn confirm_archive(ed: &mut Editor, id: AgentId) {
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    let Some(w) = &a.worktree else {
        ed.error("the agent has no worktree");
        return;
    };
    if a.archived {
        ed.error("the worktree is already archived");
        return;
    }
    if let Some(why) = busy(a) {
        ed.error(why);
        return;
    }
    let mut lines = vec![];
    if a.is_running() || a.tests_running() {
        lines.push(format!("Stops {} first.", a.name));
    }
    lines.push(format!("Removes the worktree {}.", w.path.display()));
    lines.push(format!(
        "Keeps the branch {}, so commits on it stay reachable.",
        w.branch
    ));
    lines.push(
        "Ignored files in it (build output, .env) are deleted without asking, as git worktree remove does."
            .into(),
    );
    lines.push(
        "Refused if the worktree has uncommitted or untracked files, or commits on no branch; you are asked again before anything is forced."
            .into(),
    );
    ed.agents.panel.overlay = Some(Overlay::Confirm(Confirm {
        title: format!("archive {}", a.name),
        lines,
        keys: vec![
            ("y".into(), "archive".into()),
            ("n".into(), "cancel".into()),
        ],
        action: ConfirmAction::Archive(id),
    }));
}

/// The outcome of a removal attempt.
struct Removal {
    result: Result<(), RemoveError>,
    /// The branch the worktree had checked out when it was checked.
    branch_now: Option<String>,
}

/// Removes the agent's worktree on a background thread. Without `force`,
/// work in it makes `maestro-git` refuse, and the user is asked again with
/// a list of what forcing would delete.
pub fn archive(ed: &mut Editor, id: AgentId, force: bool) {
    let git = ed.git.git().clone();
    let Some(w) = ed.agents.get(id).and_then(|a| a.worktree.clone()) else {
        return;
    };
    let unsaved = unsaved_under(ed, &w.path);
    if !unsaved.is_empty() {
        ed.error(format!(
            "not archived: unsaved edits in {} in that worktree; save or close them first",
            unsaved.join(", ")
        ));
        return;
    }
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    if a.is_running() || a.tests_running() {
        a.stop();
        a.log("stopped before archiving");
    }
    // A terminal program gets SIGHUP and, 200 ms later, SIGKILL on another
    // thread; removal waits until it is gone.
    let terminal = a.runtime.terminal().cloned();
    a.git_busy = Some("archiving");
    let spawned = ed.spawn_background("agent-archive", move |tx| {
        let removal = (|| {
            if let Some(t) = terminal {
                let deadline = Instant::now() + Duration::from_secs(3);
                while t.exit_status().is_none() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                if t.exit_status().is_none() {
                    return Removal {
                        result: Err(RemoveError::Git(maestro_git::GitError::InvalidArgument(
                            "the agent's program is still running after being stopped".into(),
                        ))),
                        branch_now: None,
                    };
                }
            }
            let repo = match git.discover(&w.main_root) {
                Ok(Some(repo)) => repo,
                Ok(None) => {
                    return Removal {
                        result: Err(RemoveError::Git(maestro_git::GitError::NotARepository(
                            w.main_root.clone(),
                        ))),
                        branch_now: None,
                    };
                }
                Err(e) => {
                    return Removal {
                        result: Err(RemoveError::Git(e)),
                        branch_now: None,
                    };
                }
            };
            let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            let branch_now = repo.worktrees().ok().and_then(|list| {
                list.into_iter()
                    .find(|x| canon(&x.path) == canon(&w.path))
                    .and_then(|x| x.branch)
            });
            Removal {
                result: repo.remove_worktree(&w.path, force),
                branch_now,
            }
        })();
        tx.send(move |ed| archived(ed, id, force, removal));
    });
    if let Err(e) = spawned {
        if let Some(a) = ed.agents.get_mut(id) {
            a.git_busy = None;
        }
        ed.error(format!("could not start archiving: {e}"));
    }
}

fn archived(ed: &mut Editor, id: AgentId, forced: bool, removal: Removal) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    a.git_busy = None;
    let Some(w) = a.worktree.clone() else {
        return;
    };
    match removal.result {
        Ok(()) => {
            a.archived = true;
            a.review = Default::default();
            let how = if forced { "force-removed" } else { "removed" };
            a.log(format!("worktree {how}; branch {} kept", w.branch));
            if let Some(v) = &ed.agents.panel.review
                && v.agent == id
            {
                ed.agents.panel.review = None;
            }
            ed.info(format!(
                "archived {}: worktree {how}, branch {} kept",
                w.path.display(),
                w.branch
            ));
        }
        Err(RemoveError::Refused(reasons)) if !forced => {
            let blocking: Vec<&RemoveRefusal> = reasons
                .iter()
                .filter(|r| {
                    matches!(
                        r,
                        RemoveRefusal::NotAWorktree
                            | RemoveRefusal::MainWorktree
                            | RemoveRefusal::Locked(_)
                    )
                })
                .collect();
            if !blocking.is_empty() {
                let why: Vec<String> = blocking.iter().map(|r| r.to_string()).collect();
                a.log(format!("archive refused: {}", why.join("; ")));
                ed.error(format!("not archived: {}", why.join("; ")));
                return;
            }
            let mut lines = vec![format!(
                "The worktree {} still holds work. Forcing deletes:",
                w.path.display()
            )];
            let mut kept = Vec::new();
            for r in &reasons {
                match r {
                    RemoveRefusal::Uncommitted(p) => lines.push(format!(
                        "  {} file(s) with uncommitted changes: {}",
                        p.len(),
                        list_paths(p)
                    )),
                    RemoveRefusal::Untracked(p) => lines.push(format!(
                        "  {} untracked file(s): {}",
                        p.len(),
                        list_paths(p)
                    )),
                    // The branch survives removal, so commits on it do too.
                    // Commits on a detached HEAD do not.
                    RemoveRefusal::Unpushed { count, .. } => match &removal.branch_now {
                        Some(b) => kept.push(format!(
                            "Not deleted: {count} commit(s) only on branch {b}, which is kept."
                        )),
                        None => lines.push(format!(
                            "  {count} commit(s) on no branch (the worktree is detached); they become unreachable"
                        )),
                    },
                    _ => {}
                }
            }
            lines.push("  ignored files in it (build output, .env)".into());
            lines.extend(kept);
            a.log("archive refused: the worktree holds work");
            ed.agents.panel.overlay = Some(Overlay::Confirm(Confirm {
                title: "force archive".into(),
                lines,
                keys: vec![
                    ("F".into(), "delete them and remove the worktree".into()),
                    ("n".into(), "keep the worktree".into()),
                ],
                action: ConfirmAction::ForceArchive(id),
            }));
        }
        Err(e) => {
            a.log(format!("archive failed: {e}"));
            ed.error(format!("not archived: {e}"));
        }
    }
}

// ----- integrate -------------------------------------------------------------

/// What integration will do, gathered before the dialog opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrateFacts {
    pub name: String,
    pub branch: String,
    pub worktree: PathBuf,
    pub main_root: PathBuf,
    /// The commit the agent's branch started from.
    pub base: String,
    /// The main worktree's checked-out branch.
    pub target: String,
    /// Uncommitted or untracked files in the agent's worktree when checked.
    pub uncommitted: Vec<PathBuf>,
    pub message: String,
    pub agent_running: bool,
}

/// The repository whose root is exactly `path`. Discovery alone would
/// climb to a parent repository when the worktree directory is gone.
fn open_exact(git: &maestro_git::Git, path: &Path) -> Result<maestro_git::Repository, String> {
    let gone = || format!("{} is no longer a worktree", path.display());
    let repo = git
        .discover(path)
        .map_err(|e| e.to_string())?
        .ok_or_else(gone)?;
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if canon(repo.root()) != canon(path) {
        return Err(gone());
    }
    Ok(repo)
}

/// Everything integration needs to hold before anything is committed:
/// both worktrees exist and are on the expected branches, the main
/// worktree is clean, and the agent's base commit is on the target branch
/// (so integrating brings only what the review showed, plus later commits
/// on the agent's branch). Returns the agent's and the main repositories.
fn check_before_commit(
    git: &maestro_git::Git,
    f: &IntegrateFacts,
) -> Result<(maestro_git::Repository, maestro_git::Repository), String> {
    let wt = open_exact(git, &f.worktree)?;
    match wt.head().map_err(|e| e.to_string())? {
        Head::Branch(b) if b == f.branch => {}
        other => {
            return Err(format!(
                "the agent's worktree is on {}, not {}",
                other.label(),
                f.branch
            ));
        }
    }
    let main = open_exact(git, &f.main_root)?;
    match main.head().map_err(|e| e.to_string())? {
        Head::Branch(b) if b == f.target => {}
        Head::Detached(_) => {
            return Err("the main worktree is not on a branch (detached HEAD)".into());
        }
        other => {
            return Err(format!(
                "the main worktree is now on {}, not {}",
                other.label(),
                f.target
            ));
        }
    }
    let dirty: Vec<PathBuf> = main
        .status()
        .map_err(|e| e.to_string())?
        .uncommitted()
        .map(|e| e.path.clone())
        .collect();
    if !dirty.is_empty() {
        return Err(format!(
            "the main worktree has uncommitted changes in {}; commit or stash them first",
            list_paths(&dirty)
        ));
    }
    if !main
        .can_fast_forward(&f.base, "HEAD")
        .map_err(|e| e.to_string())?
    {
        return Err(format!(
            "the agent started from {}, which is not on {}; integrating would also bring in \
             the commits between them, which the review does not show",
            super::create::short(&f.base),
            f.target
        ));
    }
    Ok((wt, main))
}

/// `I`: checks what integration would do on a background thread, then
/// asks. A dirty or detached main worktree is refused here, before
/// anything is committed.
pub fn prepare_integrate(ed: &mut Editor, id: AgentId) {
    let git = ed.git.git().clone();
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    let Some(w) = a.worktree.clone() else {
        ed.error("the agent has no worktree yet");
        return;
    };
    if a.archived {
        ed.error("the agent's worktree was archived; there is nothing to integrate");
        return;
    }
    if a.base_status() == Status::Rejected {
        ed.error("the agent was rejected; its work is not integrated");
        return;
    }
    if let Some(why) = busy(a) {
        ed.error(why);
        return;
    }
    if let Some(msg) = unsaved_block(ed, &w) {
        ed.error(msg);
        return;
    }
    let message = format!(
        "{}\n\nTask: {}\nMaestro agent: {} ({})",
        super::task_summary(&a.task),
        a.task.trim(),
        a.name,
        a.provider.name
    );
    let name = a.name.clone();
    let agent_running = a.is_running();
    let spawned = ed.spawn_background("agent-integrate-check", move |tx| {
        let facts = (|| -> Result<IntegrateFacts, String> {
            let main = open_exact(&git, &w.main_root)?;
            let Head::Branch(target) = main.head().map_err(|e| e.to_string())? else {
                return Err("the main worktree is not on a branch (detached HEAD)".into());
            };
            let mut facts = IntegrateFacts {
                name,
                branch: w.branch.clone(),
                worktree: w.path.clone(),
                main_root: w.main_root.clone(),
                base: w.base.clone(),
                target,
                uncommitted: Vec::new(),
                message,
                agent_running,
            };
            let (wt, _) = check_before_commit(&git, &facts)?;
            let st = wt.status().map_err(|e| e.to_string())?;
            facts.uncommitted = st
                .uncommitted()
                .chain(st.untracked())
                .map(|e| e.path.clone())
                .collect();
            Ok(facts)
        })();
        tx.send(move |ed| match facts {
            Ok(f) => {
                ed.agents.panel.overlay = Some(Overlay::Confirm(integrate_dialog(
                    id,
                    Integration::Merge,
                    f,
                )));
            }
            Err(e) => {
                if let Some(a) = ed.agents.get_mut(id) {
                    a.log(format!("integrate refused: {e}"));
                }
                ed.error(format!("not integrated, nothing changed: {e}"));
            }
        });
    });
    if let Err(e) = spawned {
        ed.error(format!("could not start the integration check: {e}"));
    }
}

/// Unsaved buffers that integration would leave out or overwrite.
fn unsaved_block(ed: &Editor, w: &super::WorktreeInfo) -> Option<String> {
    let mut unsaved = unsaved_under(ed, &w.path);
    unsaved.extend(unsaved_under(ed, &w.main_root));
    unsaved.dedup();
    (!unsaved.is_empty()).then(|| {
        format!(
            "not integrated, nothing changed: unsaved edits in {}; save or close them first",
            unsaved.join(", ")
        )
    })
}

fn how_label(how: Integration) -> &'static str {
    match how {
        Integration::Merge => "merge",
        Integration::FastForwardOnly => "fast-forward",
        Integration::CherryPick => "cherry-pick",
    }
}

pub fn integrate_dialog(agent: AgentId, how: Integration, facts: IntegrateFacts) -> Confirm {
    let mut lines = vec![];
    let subject = facts.message.lines().next().unwrap_or_default();
    if facts.uncommitted.is_empty() {
        lines.push(format!(
            "1. The agent's worktree had nothing uncommitted when checked; anything new by then is committed to {} as \"{subject}\".",
            facts.branch
        ));
    } else {
        const SHOWN: usize = 8;
        let mut names: Vec<String> = facts
            .uncommitted
            .iter()
            .take(SHOWN)
            .map(|p| p.display().to_string())
            .collect();
        if facts.uncommitted.len() > SHOWN {
            names.push(format!("and {} more", facts.uncommitted.len() - SHOWN));
        }
        lines.push(format!(
            "1. Commit all uncommitted and untracked files in {} to branch {} as \"{subject}\". When checked: {}. (D in the review drops a file you do not want.)",
            facts.worktree.display(),
            facts.branch,
            names.join(", "),
        ));
    }
    let step = match how {
        Integration::Merge => format!(
            "2. Merge {} into {} with a merge commit.",
            facts.branch, facts.target
        ),
        Integration::FastForwardOnly => format!(
            "2. Fast-forward {} to {} (refused if {} has moved on).",
            facts.target, facts.branch, facts.target
        ),
        Integration::CherryPick => format!(
            "2. Copy the commits of {} onto {} (cherry-pick).",
            facts.branch, facts.target
        ),
    };
    lines.push(format!(
        "{} The main worktree is {}.",
        step,
        facts.main_root.display()
    ));
    lines.push(
        "Before step 1 Maestro checks both branches and the main worktree again, and stops on a change. Git then refuses on conflicts."
            .into(),
    );
    lines.push(
        "Like git merge, this can overwrite ignored files in the main worktree that the agent's branch adds as tracked files."
            .into(),
    );
    if facts.agent_running {
        lines.push(
            "The agent is still running: what it changes after the commit is left out.".into(),
        );
    }
    Confirm {
        title: format!("integrate {} ({})", facts.name, how_label(how)),
        lines,
        keys: vec![
            ("y".into(), format!("integrate by {}", how_label(how))),
            ("tab".into(), "change method".into()),
            ("n".into(), "cancel".into()),
        ],
        action: ConfirmAction::Integrate { agent, how, facts },
    }
}

/// Commits the worktree's work (if any) and integrates its branch, on a
/// background thread.
pub fn integrate(ed: &mut Editor, id: AgentId, how: Integration, facts: IntegrateFacts) {
    let git = ed.git.git().clone();
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    let Some(w) = a.worktree.clone() else {
        return;
    };
    if let Some(why) = busy(a) {
        ed.error(why);
        return;
    }
    if let Some(msg) = unsaved_block(ed, &w) {
        ed.error(msg);
        return;
    }
    if let Some(a) = ed.agents.get_mut(id) {
        a.git_busy = Some("integrating");
    }
    let spawned = ed.spawn_background("agent-integrate", move |tx| {
        let result = run_integration(&git, &facts, how);
        tx.send(move |ed| integrated(ed, id, how, result));
    });
    if let Err(e) = spawned {
        if let Some(a) = ed.agents.get_mut(id) {
            a.git_busy = None;
        }
        ed.error(format!("could not start integrating: {e}"));
    }
}

/// The result of an integration attempt.
#[derive(Debug)]
struct Integrated {
    /// The commit made in the agent's worktree, if one was.
    committed: Option<String>,
    target: String,
    outcome: Result<maestro_git::IntegrateOutcome, String>,
}

fn run_integration(git: &maestro_git::Git, f: &IntegrateFacts, how: Integration) -> Integrated {
    let mut out = Integrated {
        committed: None,
        target: f.target.clone(),
        outcome: Err(String::new()),
    };
    let (wt, main) = match check_before_commit(git, f) {
        Ok(r) => r,
        Err(e) => {
            out.outcome = Err(format!("{e}; nothing changed"));
            return out;
        }
    };
    match wt.commit_all(&f.message) {
        Ok(c) => out.committed = c,
        Err(maestro_git::GitError::Unmerged(p)) => {
            out.outcome = Err(format!(
                "the agent's worktree has files in conflict: {}",
                list_paths(&p)
            ));
            return out;
        }
        Err(e) => {
            out.outcome = Err(format!("committing the agent's work failed: {e}"));
            return out;
        }
    }
    out.outcome = main
        .integrate(&f.branch, how)
        .map_err(|e| integrate_words(&e, &f.target));
    out
}

/// Plain words for a refusal or failure.
pub fn integrate_words(e: &IntegrateError, target: &str) -> String {
    match e {
        IntegrateError::Dirty(p) => format!(
            "the main worktree has uncommitted changes in {}; commit or stash them first",
            list_paths(p)
        ),
        IntegrateError::SourceDirty(p) => format!(
            "the agent's worktree still has uncommitted or untracked files: {}",
            list_paths(p)
        ),
        IntegrateError::Conflicts(p) => {
            format!("integrating would conflict in {}", list_paths(p))
        }
        IntegrateError::NotFastForward => format!(
            "{target} has commits the agent's branch lacks, so it cannot be fast-forwarded; \
             choose merge or cherry-pick (tab)"
        ),
        IntegrateError::NothingToIntegrate => {
            format!("the agent's branch has nothing that {target} lacks")
        }
        IntegrateError::DetachedHead => "the main worktree is not on a branch".into(),
        IntegrateError::InProgress(op) => {
            format!("a {op} is already in progress in the main worktree")
        }
        IntegrateError::MergeCommits(c) => format!(
            "the agent's branch has {} merge commit(s); choose merge instead",
            c.len()
        ),
        IntegrateError::Git(g) => {
            format!("git failed ({g}); check the main worktree with git status before going on")
        }
        IntegrateError::AbortFailed { cause, detail } => format!(
            "git failed ({cause}) and the main worktree could not be put back: {detail}. \
             Look at it with git status before doing anything else"
        ),
        IntegrateError::TargetChanged => {
            "the main worktree changed during the checks; nothing was integrated".into()
        }
        IntegrateError::Unexpected(s) => format!("git reported success, but {s}"),
    }
}

fn integrated(ed: &mut Editor, id: AgentId, how: Integration, r: Integrated) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    a.git_busy = None;
    if let Some(c) = &r.committed {
        a.log(format!(
            "committed the worktree's work as {}",
            super::create::short(c)
        ));
    }
    match r.outcome {
        Ok(o) => {
            a.decide(Status::Integrated);
            let msg = format!(
                "integrated {} into {} by {}: {} new commit(s), now at {}",
                a.name,
                r.target,
                how_label(how),
                o.commits.len(),
                super::create::short(&o.new_head)
            );
            a.log(msg.clone());
            ed.info(msg);
        }
        Err(e) => {
            let mut msg = format!("not integrated: {e}");
            if let Some(c) = &r.committed {
                msg.push_str(&format!(
                    " (the agent's work was committed on its branch as {})",
                    super::create::short(c)
                ));
            }
            a.log(msg.clone());
            ed.error(msg);
        }
    }
    super::review::request(ed, id);
}
