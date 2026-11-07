//! What the user can do with an agent: run its tests, send it a message,
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
use super::{AgentId, Origin, ProviderKind, ProviderSpec, Runtime, Status, TestFinish, TestRun};
use crate::editor::{Editor, normalize_path};

/// Lists at most five paths, then how many more.
pub(crate) fn list_paths(paths: &[PathBuf]) -> String {
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

/// A found worktree has no record: only review, archive and adopt apply.
pub(crate) fn found_refusal(a: &super::Agent) -> Option<String> {
    (a.origin == Origin::Found).then(|| {
        format!(
            "{} is a found worktree with no record; a adopts it first (ret reviews it, A archives it)",
            a.name
        )
    })
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

/// Why `t` refuses for the agent, judged from the record and the config
/// (and whether the worktree has a `Cargo.toml`). [`run_tests`] refuses
/// with it, after checking the worktree's branch on disk; the panel's
/// hints use it, so they never offer a test run that this refuses.
pub(crate) fn test_blocker(ed: &Editor, a: &super::Agent) -> Option<String> {
    if let Some(why) = test_record_blocker(a) {
        return Some(why);
    }
    let w = a.worktree.as_ref()?;
    let from_contract = a.contract.as_ref().is_some_and(|c| c.verify.is_some());
    if !from_contract && test_command(ed, &w.path).is_none() {
        return Some(NO_TEST_COMMAND.into());
    }
    None
}

/// The part of [`test_blocker`] that only reads the record.
fn test_record_blocker(a: &super::Agent) -> Option<String> {
    if a.worktree.is_none() {
        return Some("the agent has no worktree yet".into());
    }
    if a.archived {
        return Some("the agent's worktree was archived".into());
    }
    if let Some(why) = found_refusal(a) {
        return Some(why);
    }
    if let Some(note) = &a.worktree_note {
        return Some(format!("tests not run: {note}"));
    }
    if a.tests_running() {
        return Some("tests are already running for this agent".into());
    }
    None
}

const NO_TEST_COMMAND: &str =
    "no test command: the worktree has no Cargo.toml; set [agents] test-command";

/// `t`: runs the verification command in the agent's worktree.
pub fn run_tests(ed: &mut Editor, id: AgentId) {
    let tx = ed.idle_job_sender();
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    if let Some(why) = test_record_blocker(a) {
        ed.error(why);
        return;
    }
    let Some(root) = a.worktree.as_ref().map(|w| w.path.clone()) else {
        return;
    };
    // Checked again now: the worktree may have changed since it was made
    // or loaded, and the tests run code from it.
    if let Some(why) = a.worktree.as_ref().and_then(super::worktree_mismatch) {
        ed.error(format!("tests not run: {why}"));
        return;
    }
    if let Some(why) = test_blocker(ed, a) {
        ed.error(why);
        return;
    }
    // The task contract's command comes first, for this task only.
    let from_contract = a.contract.as_ref().and_then(|c| c.verify.clone());
    let source = if from_contract.is_some() {
        " (from the task contract)"
    } else {
        ""
    };
    let agent_running = a.is_running();
    let Some(cmd) = from_contract.or_else(|| test_command(ed, &root)) else {
        ed.error(NO_TEST_COMMAND);
        return;
    };
    let spec = maestro_tasks::TaskSpec {
        program: cmd[0].clone(),
        args: cmd[1..].to_vec(),
        cwd: root,
    };
    let shown = cmd.join(" ");
    let worktree = a.worktree.clone();
    let git = ed.git.git().clone();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    let started = Instant::now();
    a.tests.push(TestRun {
        command: shown.clone(),
        started,
        output: VecDeque::new(),
        dropped: 0,
        finished: None,
        tree: None,
        agent_was_running: agent_running,
    });
    a.test_cancel = cancel.clone();
    a.log(format!("tests started{source}: {shown}"));
    ed.agents.panel.view = DetailView::Tests;
    ed.agents.panel.scroll = 0;
    ed.info(format!("running {shown}{source}"));
    // On a background thread: first a snapshot of the files the run
    // starts on (what integrate later compares against), then the run.
    let spawned = ed.spawn_background("agent-tests", move |job| {
        let tree = match &worktree {
            Some(w) => open_agent_worktree(&git, w)
                .and_then(|repo| repo.work_snapshot().map_err(|e| e.to_string()))
                .map(|s| s.tree),
            None => Err("no worktree".into()),
        };
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            job.send(move |ed| test_not_started(ed, id, started, "stopped before it started"));
            return;
        }
        // Lines collect here and one job drains them, so a chatty test
        // run cannot queue unbounded jobs: the buffer keeps at most
        // MAX_TEST_LINES lines and counts the rest as dropped.
        let buffer: Arc<Mutex<OutputBuffer>> = Default::default();
        let result = maestro_tasks::start(&spec, move |event| match event {
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
        job.send(move |ed| test_started(ed, id, started, tree, result));
    });
    if let Err(e) = spawned {
        test_not_started(ed, id, started, &format!("could not start: {e}"));
    }
}

/// The run's process started (or failed to): keep its handle, or kill it
/// at once if the agent was stopped in the meantime.
fn test_started(
    ed: &mut Editor,
    id: AgentId,
    started: Instant,
    tree: Result<String, String>,
    result: std::io::Result<maestro_tasks::TaskHandle>,
) {
    let handle = match result {
        Ok(h) => h,
        Err(e) => return test_not_started(ed, id, started, &e.to_string()),
    };
    let Some(a) = ed.agents.get_mut(id) else {
        handle.kill();
        return;
    };
    if a.test_cancel.load(std::sync::atomic::Ordering::SeqCst) {
        handle.kill();
    }
    match tree {
        Ok(t) => {
            if let Some(run) = a.tests.iter_mut().rev().find(|r| r.started == started) {
                run.tree = Some(t);
            }
        }
        Err(e) => a.log(format!(
            "could not record which files the tests ran on: {e}"
        )),
    }
    a.test_handle = Some(handle);
}

/// The run never started: it is marked finished, killed, with the reason.
fn test_not_started(ed: &mut Editor, id: AgentId, started: Instant, why: &str) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    if let Some(run) = a
        .tests
        .iter_mut()
        .rev()
        .find(|r| r.started == started && r.finished.is_none())
    {
        let msg = format!("could not run {}: {why}", run.command);
        run.output.push_back(msg.clone());
        run.finished = Some(TestFinish {
            code: None,
            duration: Duration::ZERO,
            summary: None,
        });
        a.log(msg.clone());
        ed.error(msg);
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
            let running = a.is_running();
            if let Some(run) = a.tests.last_mut() {
                run.agent_was_running |= running;
            }
            a.test_handle = None;
            a.log(format!("tests {desc}"));
        }
    }
}

// ----- message -----------------------------------------------------------

/// Why a message cannot reach the agent now, if it cannot. `m` and `i`
/// refuse with it, and the panel's hints use it, so they never offer a
/// message that would be refused.
pub(crate) fn message_blocker(a: &super::Agent) -> Option<String> {
    if let Some(why) = found_refusal(a) {
        return Some(why);
    }
    if a.archived {
        return Some(format!(
            "{} is archived: its worktree was removed and no agent runs there to take a message",
            a.name
        ));
    }
    match &a.runtime {
        Runtime::None if a.origin == Origin::Composed => Some(format!(
            "{} is a composed result: no agent runs in it to take a message",
            a.name
        )),
        Runtime::None if a.origin == Origin::Loaded => Some(match a.kind() {
            ProviderKind::Terminal => format!(
                "{} is from an earlier session and its program is not running, so a message cannot \
                 reach it. R relaunches it without its task; then i types into it",
                a.name
            ),
            ProviderKind::Acp => format!(
                "{} is from an earlier session and not running, so a message cannot reach it. \
                 R relaunches it; then m sends the message",
                a.name
            ),
        }),
        Runtime::None => Some("the agent is not running; a message cannot reach it".into()),
        Runtime::Acp(_) if a.exited.is_some() => {
            Some("the agent process has exited; a message cannot reach it".into())
        }
        Runtime::Acp(_) if a.turn_running => {
            Some("the agent is still working on a turn; send the message when it ends".into())
        }
        Runtime::Terminal(t) if t.exit_status().is_some() => {
            Some("the agent's program has exited; a message cannot reach it".into())
        }
        _ => None,
    }
}

/// `m`: asks for a message to send the agent, with the unsent review
/// comments.
pub fn ask_message(ed: &mut Editor, id: AgentId) {
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    if let Some(why) = message_blocker(a) {
        ed.error(why);
        return;
    }
    let comments = a.comments.iter().filter(|c| !c.sent).count();
    let label = match a.kind() {
        ProviderKind::Acp => {
            format!("message to the agent (a new prompt, with {comments} comment(s))")
        }
        ProviderKind::Terminal => format!(
            "message to the agent (typed into its terminal with {comments} comment(s), then Enter)"
        ),
    };
    ed.agents.panel.overlay = Some(Overlay::Input {
        purpose: InputPurpose::Message(id),
        label,
        text: String::new(),
    });
}

/// What `m` sends: unsent comments, then the typed text.
pub fn message_text(a: &super::Agent, text: &str) -> String {
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

pub fn send_message(ed: &mut Editor, id: AgentId, text: &str) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    if let Some(why) = message_blocker(a) {
        ed.error(why);
        return;
    }
    let msg = message_text(a, text);
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
            "sent the message as a new prompt".to_string()
        }),
        Runtime::Terminal(t) => {
            let mut bytes = maestro_term::encode_paste(&msg, t.bracketed_paste());
            bytes.push(b'\r');
            t.write(&bytes).map(|()| {
                "typed the message into the terminal; Maestro cannot tell whether the program reads it"
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
            let msg = format!("the message was not sent: {e}");
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
pub(crate) fn busy(a: &super::Agent) -> Option<String> {
    a.git_busy
        .map(|what| format!("{} is busy ({what}); wait for it to finish", a.name))
}

/// Why `X` refuses for the agent. [`confirm_reject`] refuses with it and
/// the panel's hints use it.
pub(crate) fn reject_blocker(a: &super::Agent) -> Option<String> {
    if let Some(why) = found_refusal(a) {
        return Some(why);
    }
    if a.archived {
        return Some(format!(
            "{} is archived: its worktree was removed, so there is nothing left to reject",
            a.name
        ));
    }
    if a.base_status() == Status::Rejected {
        return Some("the agent is already rejected; A archives its worktree".into());
    }
    if a.base_status() == Status::Integrated {
        return Some(
            "the agent's work is already integrated; rejecting it now changes nothing".into(),
        );
    }
    // Rejecting while the worktree is still being made is allowed: the
    // agent is then never launched.
    busy(a).filter(|_| a.git_busy != Some(super::CREATING))
}

/// `X`: asks before rejecting.
pub fn confirm_reject(ed: &mut Editor, id: AgentId) {
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    if let Some(why) = reject_blocker(a) {
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
    if let Some(why) = found_refusal(a) {
        ed.error(why);
        return;
    }
    let Some(w) = a.worktree.clone() else {
        return;
    };
    if let Some(why) = busy(a) {
        ed.error(why);
        return;
    }
    let base = &w.base[..w.base.len().min(8)];
    let shown = |p: &Option<PathBuf>| p.as_deref().map(|p| p.display().to_string());
    let (old, new) = (file.old_path.clone(), file.new_path.clone());
    // Git shows a file replaced by a symlink (or the reverse) as a deletion
    // and an addition of the same path; dropping either restores it.
    let type_change = old.is_none()
        && a.review
            .files
            .iter()
            .any(|f| f.new_path.is_none() && f.old_path == new);
    let what = match (shown(&old), shown(&new)) {
        (None, Some(n)) if type_change => format!(
            "{n} goes back to its version at {base}, the commit the agent started from (the agent changed its type)."
        ),
        (Some(o), Some(n)) if o == n => {
            format!("{n} goes back to its version at {base}, the commit the agent started from.")
        }
        (Some(o), Some(n)) => {
            format!("{n} is deleted and {o} comes back as it was at {base}.")
        }
        (None, Some(n)) => format!("{n} is deleted; it did not exist at {base}."),
        (Some(o), None) => format!("{o} comes back as it was at {base}."),
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
    // Drop is Git work: it must not overlap integrating or archiving, and
    // quitting waits for it.
    match ed.agents.get_mut(id) {
        Some(a) if a.git_busy.is_none() => a.git_busy = Some("dropping a file"),
        Some(a) => {
            let why = busy(a).unwrap_or_default();
            ed.error(why);
            return;
        }
        None => return,
    }
    let spawned = ed.spawn_background("agent-drop", move |tx| {
        let result = open_agent_worktree(&git, &w).and_then(|repo| {
            // The dialog promised the version at the agent's starting
            // commit; the branch must still descend from it.
            let head = repo.resolve("HEAD").map_err(|e| e.to_string())?;
            if !repo
                .can_fast_forward(&w.base, &head)
                .map_err(|e| e.to_string())?
            {
                return Err(format!(
                    "{} no longer descends from the starting commit {}; nothing changed",
                    w.branch, w.base
                ));
            }
            repo.drop_change(&w.base, old.as_deref(), new.as_deref())
                .map_err(|e| e.to_string())
        });
        tx.send(move |ed| {
            if let Some(a) = ed.agents.get_mut(id) {
                a.git_busy = None;
            }
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
        if let Some(a) = ed.agents.get_mut(id) {
            a.git_busy = None;
        }
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
    super::conflicts::refresh(ed);
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

/// Why `A` refuses for the agent. [`confirm_archive`] refuses with it and
/// the panel's hints use it.
pub(crate) fn archive_blocker(a: &super::Agent) -> Option<String> {
    let Some(w) = &a.worktree else {
        return Some("the agent has no worktree".into());
    };
    if a.archived {
        return Some(format!(
            "the worktree is already archived; d forgets {}'s record",
            a.name
        ));
    }
    if let Some(why) = busy(a) {
        return Some(why);
    }
    // A directory that is gone has nothing left to delete: archiving only
    // marks the record (or lets Git drop its stale entry).
    if let Some(note) = a.worktree_note.as_ref().filter(|_| w.path.exists()) {
        return Some(format!(
            "not archived: {note}. Maestro does not remove a worktree that changed since it was \
             recorded; look at it with `git worktree list`"
        ));
    }
    None
}

/// `A`: asks before removing the worktree.
pub fn confirm_archive(ed: &mut Editor, id: AgentId) {
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    if let Some(why) = archive_blocker(a) {
        ed.error(why);
        return;
    }
    let Some(w) = &a.worktree else {
        return;
    };
    let mut lines = vec![];
    if a.origin == Origin::Found {
        lines.push(format!(
            "{} has no record in Maestro: its task and provider are unknown.",
            w.path.display()
        ));
    }
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
    /// The directory was gone and Git did not list it: nothing to remove.
    already_gone: bool,
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
                        already_gone: false,
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
                        already_gone: false,
                    };
                }
                Err(e) => {
                    return Removal {
                        result: Err(RemoveError::Git(e)),
                        branch_now: None,
                        already_gone: false,
                    };
                }
            };
            let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            let list = match repo.worktrees() {
                Ok(list) => list,
                Err(e) => {
                    return Removal {
                        result: Err(RemoveError::Git(e)),
                        branch_now: None,
                        already_gone: false,
                    };
                }
            };
            let entry = list.into_iter().find(|x| canon(&x.path) == canon(&w.path));
            let refuse = |msg: String| Removal {
                result: Err(RemoveError::Git(maestro_git::GitError::InvalidArgument(
                    msg,
                ))),
                branch_now: None,
                already_gone: false,
            };
            let branch_now = match entry {
                // Removed already (by hand, or by another Maestro): there
                // is nothing left to delete, so the record is only marked.
                None if !w.path.exists() => {
                    return Removal {
                        result: Ok(()),
                        branch_now: None,
                        already_gone: true,
                    };
                }
                None => {
                    return refuse(format!(
                        "{} is not a worktree of this repository; nothing was removed",
                        w.path.display()
                    ));
                }
                // The path now holds other work than the record says: a
                // removal would delete what the dialog did not name.
                Some(x) if x.branch.as_deref() != Some(w.branch.as_str()) => {
                    return refuse(format!(
                        "the worktree {} is on {} now, not {}; nothing was removed",
                        w.path.display(),
                        x.branch
                            .as_deref()
                            .map_or("a detached HEAD".to_string(), |b| format!("branch {b}")),
                        w.branch
                    ));
                }
                Some(x) => x.branch,
            };
            Removal {
                result: repo.remove_worktree(&w.path, force),
                branch_now,
                already_gone: false,
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
    archived_inner(ed, id, forced, removal);
    super::conflicts::refresh(ed);
}

fn archived_inner(ed: &mut Editor, id: AgentId, forced: bool, removal: Removal) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    a.git_busy = None;
    let Some(w) = a.worktree.clone() else {
        return;
    };
    match removal.result {
        Ok(()) if removal.already_gone => {
            a.archived = true;
            a.review = Default::default();
            a.log(format!(
                "the worktree {} was already gone; marked archived, branch {} kept",
                w.path.display(),
                w.branch
            ));
            if let Some(v) = &ed.agents.panel.review
                && v.agent == id
            {
                ed.agents.panel.review = None;
            }
            ed.info(format!(
                "{} was already gone; marked archived, nothing removed",
                w.path.display()
            ));
        }
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
    /// The agent's task contract.
    pub contract: Option<super::contract::Contract>,
    /// Changed files the contract does not allow, with the reason, from
    /// the snapshot the dialog was checked on. Integrating them needs `O`;
    /// after the commit, any other file outside the contract stops the
    /// integration.
    pub out_of_scope: Vec<(PathBuf, String)>,
    /// Why the last verification does not vouch for these files, if it
    /// does not (failed, still running, none, or run on other files).
    pub verification: Vec<String>,
    /// Other agents whose work, merged after this one, would conflict with
    /// the target branch: `name: files`.
    pub conflicts_after: Vec<String>,
    /// Other agents with overlapping files whose work would still merge.
    pub clean_after: Vec<String>,
    /// Dry runs that could not run, `name: why`.
    pub unchecked: Vec<String>,
    /// Files in which this agent's own work conflicts with the target
    /// branch now, from a dry run; `maestro-git` refuses such a merge.
    pub own_conflicts: Vec<PathBuf>,
    /// The tree of the agent's files (committed, uncommitted and
    /// untracked) when the dialog was checked; every list and warning in
    /// the dialog comes from this one snapshot.
    pub checked_tree: String,
    /// The commit the agent's branch pointed at then. What is integrated
    /// is this commit, or the commit Maestro makes on top of it, and only
    /// when that holds exactly `checked_tree` and the branch still points
    /// at it right before the merge. Anything else (the agent committed,
    /// reset or rebased after the dialog was checked) integrates nothing.
    pub checked_head: String,
    /// Set when only the hunks marked in the review are integrated: they
    /// become a commit of their own on a new branch, and that commit is
    /// integrated. `checked_tree` is then that commit's tree and
    /// `checked_head` the commit the review was computed on.
    pub part: Option<Box<super::parts::PartPlan>>,
}

/// The net change of `commit` since its fork point from `base`, as the
/// review shows it.
fn diff_since_fork(
    repo: &maestro_git::Repository,
    base: &str,
    commit: &str,
) -> Result<Vec<maestro_git::DiffFile>, String> {
    repo.diff(
        &maestro_git::DiffSpec::SinceForkPoint {
            base: base.to_string(),
            head: commit.to_string(),
        },
        &[],
    )
    .map_err(|e| e.to_string())
}

/// Paths outside the contract that integrating `commit` could bring:
/// those of its net diff from the fork point and those any commit up to
/// it touches, since a file changed and changed back is invisible in the
/// net diff but a cherry-pick applies each commit. `commit` is a snapshot
/// of the worktree for the dialog and the branch's commit afterward.
pub(crate) fn contract_violations(
    c: &super::contract::Contract,
    wt: &maestro_git::Repository,
    base: &str,
    commit: &str,
) -> Result<Vec<(PathBuf, String)>, String> {
    let files = diff_since_fork(wt, base, commit)?;
    let mut out = super::out_of_scope(c, &files);
    for p in wt.paths_touched(base, commit).map_err(|e| e.to_string())? {
        if !out.iter().any(|(q, _)| *q == p)
            && let Some(why) = c.violation(&p)
        {
            out.push((p, format!("{why} (changed by a commit on the branch)")));
        }
    }
    Ok(out)
}

/// What the integrate check needs to know about another agent.
#[derive(Debug, Clone)]
struct Other {
    name: String,
    worktree: super::WorktreeInfo,
    paths: std::collections::BTreeSet<PathBuf>,
}

/// What the integrate check needs to know about the last verification.
#[derive(Debug, Clone)]
struct LastTest {
    command: String,
    /// `None` while running.
    passed: Option<bool>,
    describe: String,
    tree: Option<String>,
    agent_was_running: bool,
}

/// Plain sentences for the dialog about the last verification run, judged
/// against the files now (`tree`).
fn verification_warnings(
    last: Option<&LastTest>,
    has_command: bool,
    tree: Option<&str>,
    changed: &[PathBuf],
) -> Vec<String> {
    let Some(t) = last else {
        // Without any verification command there is nothing to warn about
        // that `t` could fix.
        return if has_command {
            vec!["No verification has run on this work (t runs it).".into()]
        } else {
            Vec::new()
        };
    };
    let mut out = Vec::new();
    match t.passed {
        None => out.push(format!(
            "The verification `{}` is still running; its result is not known.",
            t.command
        )),
        Some(false) => out.push(format!(
            "The last verification `{}` did not pass: {}.",
            t.command, t.describe
        )),
        Some(true) => {}
    }
    if t.passed.is_some() {
        match (&t.tree, tree) {
            (Some(a), Some(b)) if a == b => {}
            (Some(_), Some(_)) => out.push(format!(
                "The files changed after the last verification `{}` ran: {}.",
                t.command,
                list_paths(changed)
            )),
            _ => out.push(format!(
                "Maestro cannot tell whether the files changed after the last verification `{}` ran.",
                t.command
            )),
        }
    }
    if t.agent_was_running {
        out.push(
            "The agent was running during the last verification and may have changed files it did not see."
                .into(),
        );
    }
    out
}

/// The repository whose root is exactly `path`. Discovery alone would
/// climb to a parent repository when the worktree directory is gone.
pub(crate) fn open_exact(
    git: &maestro_git::Git,
    path: &Path,
) -> Result<maestro_git::Repository, String> {
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

/// Both are worktrees of one repository (they share a Git directory). A
/// recorded path now holding another clone with a branch of the same name
/// fails this.
fn same_repository(
    wt: &maestro_git::Repository,
    main: &maestro_git::Repository,
) -> Result<(), String> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if canon(wt.common_dir()) == canon(main.common_dir()) {
        Ok(())
    } else {
        Err(format!(
            "{} is not a worktree of the repository at {}",
            wt.root().display(),
            main.root().display()
        ))
    }
}

/// The agent's worktree, checked before changing anything in it: the root
/// of a worktree of the main repository, on the branch the record names.
pub(crate) fn open_agent_worktree(
    git: &maestro_git::Git,
    w: &super::WorktreeInfo,
) -> Result<maestro_git::Repository, String> {
    let wt = open_exact(git, &w.path)?;
    match wt.head().map_err(|e| e.to_string())? {
        Head::Branch(b) if b == w.branch => {}
        other => {
            return Err(format!(
                "the agent's worktree is on {}, not {}",
                other.label(),
                w.branch
            ));
        }
    }
    let main = open_exact(git, &w.main_root)?;
    same_repository(&wt, &main)?;
    Ok(wt)
}

/// Everything integration needs to hold before anything is committed:
/// both worktrees exist and are on the expected branches, the main
/// worktree is clean, and the agent's base commit is on the target branch
/// (so integrating brings only what the review showed, plus later commits
/// on the agent's branch). Returns the agent's and the main repositories.
pub(crate) fn check_before_commit(
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
    same_repository(&wt, &main)?;
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

/// Why `I` refuses for the agent, judged from the record. [`prepare_integrate`]
/// refuses with it before its other checks (unsaved buffers, then Git),
/// and the panel's hints use it.
pub(crate) fn integrate_blocker(a: &super::Agent) -> Option<String> {
    if a.worktree.is_none() {
        return Some("the agent has no worktree yet".into());
    }
    if a.archived {
        return Some("the agent's worktree was archived; there is nothing to integrate".into());
    }
    if a.base_status() == Status::Rejected {
        return Some("the agent was rejected; its work is not integrated".into());
    }
    if let Some(why) = found_refusal(a) {
        return Some(why);
    }
    busy(a)
}

/// `I`: checks what integration would do on a background thread, then
/// asks. A dirty or detached main worktree is refused here, before
/// anything is committed.
pub fn prepare_integrate(ed: &mut Editor, id: AgentId) {
    let git = ed.git.git().clone();
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    if let Some(why) = integrate_blocker(a) {
        ed.error(why);
        return;
    }
    let Some(w) = a.worktree.clone() else {
        return;
    };
    if let Some(msg) = unsaved_block(ed, &w) {
        ed.error(msg);
        return;
    }
    // With marked hunks, only they are integrated.
    if !a.marks.is_empty() {
        super::parts::prepare(ed, id);
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
    let contract = a.contract.clone();
    let has_command = a.contract.as_ref().is_some_and(|c| c.verify.is_some())
        || test_command(ed, &w.path).is_some();
    let last_test = a.latest_test().map(|t| LastTest {
        command: t.command.clone(),
        passed: t.passed(),
        describe: t.describe(),
        tree: t.tree.clone(),
        agent_was_running: t.agent_was_running,
    });
    // Other agents of this repository that could conflict afterward; only
    // those whose changed files overlap this one's get a dry run.
    let others: Vec<Other> = ed
        .agents
        .agents
        .iter()
        .filter(|o| o.id != id && super::conflicts::eligible(o))
        .filter_map(|o| {
            let ow = o.worktree.clone()?;
            (ow.main_root == w.main_root).then(|| Other {
                name: o.name.clone(),
                worktree: ow,
                paths: super::conflicts::changed_paths(o),
            })
        })
        .collect();
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
                contract,
                out_of_scope: Vec::new(),
                verification: Vec::new(),
                conflicts_after: Vec::new(),
                clean_after: Vec::new(),
                unchecked: Vec::new(),
                own_conflicts: Vec::new(),
                checked_tree: String::new(),
                checked_head: String::new(),
                part: None,
            };
            let (wt, main) = check_before_commit(&git, &facts)?;
            // One snapshot of the agent's files, from Git now rather than
            // the review's last refresh; everything below is about it.
            let snap = wt
                .work_snapshot()
                .map_err(|e| format!("could not read the agent's files: {e}"))?;
            facts.checked_tree = snap.tree.clone();
            facts.checked_head = snap.head.clone();
            let st = wt.status().map_err(|e| e.to_string())?;
            facts.uncommitted = st
                .uncommitted()
                .chain(st.untracked())
                .map(|e| e.path.clone())
                .collect();
            let files = diff_since_fork(&wt, &w.base, &snap.commit)?;
            if let Some(c) = &facts.contract {
                facts.out_of_scope = contract_violations(c, &wt, &w.base, &snap.commit)?;
            }
            let mine: std::collections::BTreeSet<PathBuf> =
                files.iter().flat_map(super::file_paths).cloned().collect();
            // The files now against the files the last verification ran on.
            let tree = Some(snap.tree.clone());
            let changed = match (last_test.as_ref().and_then(|t| t.tree.clone()), &tree) {
                (Some(from), Some(to)) if from != *to => main
                    .diff(
                        &maestro_git::DiffSpec::Revisions {
                            from,
                            to: to.clone(),
                        },
                        &[],
                    )
                    .map(|d| d.iter().flat_map(super::file_paths).cloned().collect())
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            facts.verification =
                verification_warnings(last_test.as_ref(), has_command, tree.as_deref(), &changed);
            let overlapping: Vec<&Other> = others
                .iter()
                .filter(|o| super::conflicts::paths_overlap(&o.paths, &mine))
                .collect();
            conflicts_after(&git, &main, Ok(snap), &overlapping, &mut facts);
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

/// Whether this agent's work conflicts with the target branch now, and
/// which overlapping agents would conflict with it once this work is
/// merged. The merge is made in memory: a dry run of the target's `HEAD`
/// with this worktree's files, written as an unreferenced commit, then a
/// dry run of that commit with each other agent's files.
fn conflicts_after(
    git: &maestro_git::Git,
    main: &maestro_git::Repository,
    snap: Result<maestro_git::WorkSnapshot, String>,
    others: &[&Other],
    facts: &mut IntegrateFacts,
) {
    let mut unchecked = |why: String| {
        for o in others {
            facts.unchecked.push(format!("{}: {why}", o.name));
        }
    };
    let snap = match snap {
        Ok(s) => s,
        Err(e) => return unchecked(e),
    };
    let head = match main.resolve("HEAD") {
        Ok(h) => h,
        Err(e) => return unchecked(e.to_string()),
    };
    let merged = match main.merge_check(&head, &snap.commit) {
        Ok(maestro_git::MergeCheck::Clean { .. }) if others.is_empty() => return,
        Ok(maestro_git::MergeCheck::Clean { tree }) => {
            match main.commit_tree(&tree, &[&head, &snap.commit], "maestro integrate dry run") {
                Ok(c) => c,
                Err(e) => return unchecked(e.to_string()),
            }
        }
        Ok(maestro_git::MergeCheck::Conflicts { files, .. }) => {
            facts.own_conflicts = files;
            return unchecked(format!(
                "not checked, this work itself conflicts with {}",
                facts.target
            ));
        }
        Err(e) => return unchecked(e.to_string()),
    };
    for o in others {
        let theirs = open_agent_worktree(git, &o.worktree)
            .and_then(|r| r.work_snapshot().map_err(|e| e.to_string()));
        match theirs.map(|t| main.merge_check(&merged, &t.commit)) {
            Ok(Ok(maestro_git::MergeCheck::Clean { .. })) => facts.clean_after.push(o.name.clone()),
            Ok(Ok(maestro_git::MergeCheck::Conflicts { files, .. })) => facts
                .conflicts_after
                .push(format!("{} in {}", o.name, list_paths(&files))),
            Ok(Err(e)) => facts.unchecked.push(format!("{}: {e}", o.name)),
            Err(e) => facts.unchecked.push(format!("{}: {e}", o.name)),
        }
    }
}

/// Unsaved buffers that integration would leave out or overwrite.
pub(crate) fn unsaved_block(ed: &Editor, w: &super::WorktreeInfo) -> Option<String> {
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

pub(crate) fn how_label(how: Integration) -> &'static str {
    match how {
        Integration::Merge => "merge",
        Integration::FastForwardOnly => "fast-forward",
        Integration::CherryPick => "cherry-pick",
    }
}

/// At most `n` items, then how many more.
fn first_of(items: &[String], n: usize) -> String {
    let mut v: Vec<String> = items.iter().take(n).cloned().collect();
    if items.len() > n {
        v.push(format!("and {} more", items.len() - n));
    }
    v.join("; ")
}

pub fn integrate_dialog(agent: AgentId, how: Integration, facts: IntegrateFacts) -> Confirm {
    let mut lines = vec![];
    if !facts.out_of_scope.is_empty() {
        let names: Vec<String> = facts
            .out_of_scope
            .iter()
            .map(|(_, why)| why.clone())
            .collect();
        // Every file is named, never "and N more": `O` lets in exactly
        // these, and a dialog too long for the window cannot be accepted.
        lines.push(format!(
            "OUTSIDE THE TASK CONTRACT: {} changed file(s) would be integrated: {}. y is refused; O integrates them anyway, D in the review drops one.",
            facts.out_of_scope.len(),
            names.join("; ")
        ));
    }
    if !facts.own_conflicts.is_empty() {
        lines.push(format!(
            "CONFLICT: this work conflicts with {} in {}; Git will refuse to merge it (after step 1 commits it on its branch).",
            facts.target,
            list_paths(&facts.own_conflicts)
        ));
    }
    if !facts.conflicts_after.is_empty() {
        lines.push(format!(
            "After this, the work of {} would conflict with {}.",
            first_of(&facts.conflicts_after, 5),
            facts.target
        ));
    }
    if !facts.clean_after.is_empty() {
        lines.push(format!(
            "Overlapping agents whose work would still merge cleanly afterward: {}.",
            first_of(&facts.clean_after, 5)
        ));
    }
    if !facts.unchecked.is_empty() {
        lines.push(format!(
            "Could not check for conflicts afterward with: {}.",
            first_of(&facts.unchecked, 3)
        ));
    }
    for v in &facts.verification {
        lines.push(format!("Warning: {v}"));
    }
    if let Some(part) = &facts.part {
        lines.extend(super::parts::steps(&facts, part, how));
        return finish_dialog(agent, how, facts, lines);
    }
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
            "1. Commit all uncommitted and untracked files in {} to branch {} as \"{subject}\". When checked ({} in all; the review lists every one): {}. D in the review drops a file you do not want.",
            facts.worktree.display(),
            facts.branch,
            facts.uncommitted.len(),
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
            "The agent is still running: only what this check saw can be integrated. A commit or file change Maestro sees before the merge stops the integration; a later one is left out."
                .into(),
        );
    }
    finish_dialog(agent, how, facts, lines)
}

/// The dialog's keys and title around `lines`.
fn finish_dialog(
    agent: AgentId,
    how: Integration,
    facts: IntegrateFacts,
    lines: Vec<String>,
) -> Confirm {
    let go = if facts.out_of_scope.is_empty() {
        ("y".into(), format!("integrate by {}", how_label(how)))
    } else {
        (
            "O".into(),
            format!(
                "integrate with files outside the contract by {}",
                how_label(how)
            ),
        )
    };
    let what = if facts.part.is_some() { "part of " } else { "" };
    Confirm {
        title: format!("integrate {what}{} ({})", facts.name, how_label(how)),
        lines,
        keys: vec![
            go,
            ("tab".into(), "change method".into()),
            ("n".into(), "cancel".into()),
        ],
        action: ConfirmAction::Integrate {
            agent,
            how,
            facts: Box::new(facts),
        },
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
    // A part integrates the hunks as marked when the dialog was checked;
    // the marks must still be exactly those (a refresh drops the mark of a
    // hunk the agent changed since).
    if let Some(p) = &facts.part
        && let Some(why) = super::parts::marks_changed(a, p)
    {
        ed.error(format!("not integrated, nothing changed: {why}"));
        return;
    }
    if let Some(a) = ed.agents.get_mut(id) {
        a.git_busy = Some("integrating");
    }
    let spawned = ed.spawn_background("agent-integrate", move |tx| {
        let result = match &facts.part {
            Some(p) => super::parts::run(&git, &facts, p, how),
            None => run_integration(&git, &facts, how),
        };
        let part = facts.part;
        tx.send(move |ed| match part {
            Some(p) => super::parts::integrated(ed, id, how, *p, result),
            None => integrated(ed, id, how, result),
        });
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
pub(crate) struct Integrated {
    /// The commit made in the agent's worktree, or of the marked hunks, if
    /// one was.
    pub(crate) committed: Option<String>,
    pub(crate) target: String,
    pub(crate) outcome: Result<maestro_git::IntegrateOutcome, String>,
    /// Files outside the task contract that a successful integration
    /// changed on the target and the dialog did not list.
    pub(crate) late_out_of_scope: Vec<PathBuf>,
    /// Why that check of the target could not run, if it could not.
    pub(crate) audit_error: Option<String>,
    /// Files outside the contract the user let in with `O`.
    pub(crate) accepted: Vec<PathBuf>,
}

impl Integrated {
    pub(crate) fn new(f: &IntegrateFacts) -> Self {
        Self {
            committed: None,
            target: f.target.clone(),
            outcome: Err(String::new()),
            late_out_of_scope: Vec::new(),
            audit_error: None,
            accepted: f.out_of_scope.iter().map(|(p, _)| p.clone()).collect(),
        }
    }
}

fn run_integration(git: &maestro_git::Git, f: &IntegrateFacts, how: Integration) -> Integrated {
    let mut out = Integrated::new(f);
    let (wt, main) = match check_before_commit(git, f) {
        Ok(r) => r,
        Err(e) => {
            out.outcome = Err(format!("{e}; nothing changed"));
            return out;
        }
    };
    let mut message = f.message.clone();
    if !f.out_of_scope.is_empty() {
        let names: Vec<String> = f
            .out_of_scope
            .iter()
            .map(|(p, _)| p.display().to_string())
            .collect();
        message.push_str(&format!(
            "\nOutside the task contract, integrated after a separate confirmation (O): {}",
            names.join(", ")
        ));
    }
    match wt.commit_all(&message) {
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
    // The dialog's lists and warnings were about one snapshot: the
    // branch at `checked_head` plus the files on disk. What may be
    // integrated is that commit, or Maestro's commit of those files on top
    // of it, and nothing else.
    let reviewed = match reviewed_commit(&wt, f, out.committed.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            out.outcome = Err(e);
            return out;
        }
    };
    land(&wt, &main, f, &f.branch, &reviewed, how, &mut out);
    out
}

/// Checks `reviewed` (the commit `branch` points at) against the task
/// contract, history included, and integrates exactly it into the main
/// worktree's branch with [`maestro_git::Repository::integrate_reviewed`],
/// then checks what reached the target against the contract once more.
/// `wt` is any worktree of the repository holding `reviewed`.
pub(crate) fn land(
    wt: &maestro_git::Repository,
    main: &maestro_git::Repository,
    f: &IntegrateFacts,
    branch: &str,
    reviewed: &str,
    how: Integration,
    out: &mut Integrated,
) {
    // Judged against the contract again, history included: a file outside
    // it that the dialog did not list stops the integration.
    let accepted = |p: &PathBuf| f.out_of_scope.iter().any(|(q, _)| q == p);
    if let Some(c) = &f.contract {
        match contract_violations(c, wt, &f.base, reviewed) {
            Ok(v) => {
                let new: Vec<PathBuf> = v
                    .into_iter()
                    .map(|(p, _)| p)
                    .filter(|p| !accepted(p))
                    .collect();
                if !new.is_empty() {
                    out.outcome = Err(format!(
                        "files outside the task contract appeared after the dialog was checked: \
                         {}; nothing was integrated. Press I again to see them",
                        list_paths(&new)
                    ));
                    return;
                }
            }
            Err(e) => {
                out.outcome = Err(format!(
                    "could not check the work against the task contract ({e}); nothing was \
                     integrated"
                ));
                return;
            }
        }
    }
    let old_head = match main.resolve("HEAD") {
        Ok(h) => h,
        Err(e) => {
            out.outcome = Err(format!("could not read {} ({e})", f.target));
            return;
        }
    };
    // Only that commit: `maestro-git` refuses if the branch no longer
    // points at it, and gives git the commit id, not the branch name.
    out.outcome = main
        .integrate_reviewed(
            branch,
            maestro_git::Reviewed {
                commit: reviewed,
                target: &f.target,
                base: &f.base,
            },
            how,
        )
        .map_err(|e| integrate_words(&e, &f.target));
    // What reached the target is checked against the contract once more,
    // and anything outside it the dialog did not list is reported. It
    // should never find anything; it is a check on the checks above.
    if let (Some(c), Ok(o)) = (&f.contract, &out.outcome) {
        let landed = main.diff(
            &maestro_git::DiffSpec::Revisions {
                from: old_head,
                to: o.new_head.clone(),
            },
            &[],
        );
        match landed {
            Ok(files) => {
                out.late_out_of_scope = super::out_of_scope(c, &files)
                    .into_iter()
                    .map(|(p, _)| p)
                    .filter(|p| !accepted(p))
                    .collect();
            }
            Err(e) => out.audit_error = Some(e.to_string()),
        }
    }
}

/// The commit to integrate, after Maestro's commit of the agent's work
/// (`committed`, if it made one): it must be exactly what the dialog was
/// checked on. Without a commit of Maestro's, that is the branch's commit
/// then; with one, a commit whose only parent is that commit. Its files
/// must be the dialog's snapshot either way.
fn reviewed_commit(
    wt: &maestro_git::Repository,
    f: &IntegrateFacts,
    committed: Option<&str>,
) -> Result<String, String> {
    let shape = wt
        .commit_shape(committed.unwrap_or("HEAD"))
        .map_err(|e| format!("could not read the agent's branch ({e}); nothing was integrated"))?;
    let on_checked_head = match committed {
        Some(_) => shape.parents == [f.checked_head.as_str()],
        None => shape.commit == f.checked_head,
    };
    if !on_checked_head {
        // Someone else committed (or reset) in the agent's worktree, before
        // or while Maestro committed.
        let now = match committed {
            Some(_) => shape.parents.first().cloned().unwrap_or_default(),
            None => shape.commit.clone(),
        };
        return Err(branch_moved(&now));
    }
    if shape.tree != f.checked_tree {
        return Err(
            "the agent's files changed after the dialog was checked; nothing was \
             integrated. Press I again to see what it would bring now"
                .into(),
        );
    }
    Ok(shape.commit)
}

fn branch_moved(now: &str) -> String {
    format!(
        "the agent's branch moved after the dialog was checked (it is now at {}); nothing was \
         integrated. Press I again to see what it would bring now",
        super::create::short(now)
    )
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
        IntegrateError::SourceMoved { found, .. } => branch_moved(found),
        IntegrateError::WrongTarget { expected, found } => {
            format!("the main worktree is now on {found}, not {expected}; nothing was integrated")
        }
        IntegrateError::BaseNotOnTarget { base } => format!(
            "the agent started from {}, which is no longer on {target}; integrating would also \
             bring in commits the review does not show. Nothing was integrated",
            super::create::short(base)
        ),
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
            if !r.accepted.is_empty() {
                a.log(format!(
                    "integrated with files outside the task contract, after O: {}",
                    r.accepted
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !r.late_out_of_scope.is_empty() || r.audit_error.is_some() {
                let what = match &r.audit_error {
                    Some(e) => format!(
                        "Maestro could not check what reached {} against the task contract ({e})",
                        r.target
                    ),
                    None => format!(
                        "{} now has changes outside the task contract that the dialog did not \
                         list: {}",
                        r.target,
                        list_paths(&r.late_out_of_scope)
                    ),
                };
                let warn = format!("{msg}. WARNING: {what}; check {} with git log", r.target);
                a.log(warn.clone());
                ed.error(warn);
            } else {
                a.log(msg.clone());
                ed.info(msg);
            }
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
    // An integrated agent no longer counts for overlaps.
    super::conflicts::refresh(ed);
}

// ----- found worktrees ---------------------------------------------------------

/// Why `a` refuses for the agent. [`ask_adopt`] refuses with it and the
/// panel's hints use it.
pub(crate) fn adopt_blocker(a: &super::Agent) -> Option<String> {
    if a.origin != Origin::Found {
        return Some("a adopts a found worktree; this agent already has a record".into());
    }
    if a.archived {
        return Some("the worktree was archived".into());
    }
    busy(a)
}

/// `a` on a found worktree: asks which provider the adopted agent uses, for
/// relaunching it later.
pub fn ask_adopt(ed: &mut Editor, id: AgentId) {
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    if let Some(why) = adopt_blocker(a) {
        ed.error(why);
        return;
    }
    let providers = super::providers(ed);
    if providers.is_empty() {
        ed.error("no agent providers are configured; adopting needs one to relaunch with");
        return;
    }
    let items = providers
        .iter()
        .map(|p| (p.name.clone(), super::kind_label(p.kind).to_string()))
        .collect();
    ed.agents.panel.overlay = Some(Overlay::Choice {
        purpose: ChoicePurpose::Adopt {
            agent: id,
            providers,
        },
        title: "adopt with provider (nothing starts now)".into(),
        items,
        selected: 0,
    });
}

/// Turns a found worktree into an agent record with `provider`, saved from
/// now on. Nothing starts; `R` relaunches it.
pub fn adopt(ed: &mut Editor, id: AgentId, provider: ProviderSpec) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    if a.origin != Origin::Found {
        return;
    }
    let Some(branch) = a.worktree.as_ref().map(|w| w.branch.clone()) else {
        return;
    };
    a.origin = Origin::Loaded;
    a.name = format!("{}-{}", provider.name, id.0);
    a.task = format!("Work found on branch {branch}; Maestro has no record of its task");
    a.log(format!(
        "adopted with provider {}; its earlier status and task are unknown",
        provider.name
    ));
    a.provider = provider;
    a.decide(Status::Stopped);
    let name = a.name.clone();
    let main_root = a.worktree.as_ref().map(|w| w.main_root.clone());
    let saved = match main_root
        .as_deref()
        .and_then(|r| ed.agents.persist.blocked(r))
    {
        Some(why) => format!("it is not saved in this session ({why})"),
        None if super::persist::enabled(ed).is_none() => {
            "it is not saved: sessions are off".to_string()
        }
        None => "it is saved from now on".to_string(),
    };
    ed.info(format!(
        "adopted {branch} as {name}: {saved}; R relaunches it"
    ));
}

// ----- forgetting an archived agent's record ---------------------------------

/// Why `d` refuses to forget the agent's record. Only an archived agent's
/// record can be forgotten: its worktree was removed already, so nothing on
/// disk is lost. [`confirm_forget`] and [`forget`] refuse with it, and the
/// panel's hints use it.
pub(crate) fn forget_blocker(a: &super::Agent) -> Option<String> {
    if !a.archived {
        return Some(format!(
            "{} is not archived; d forgets only the record of an archived agent, whose worktree \
             is gone (A archives it first)",
            a.name
        ));
    }
    busy(a)
}

/// `d`: asks before forgetting an archived agent's record.
pub fn confirm_forget(ed: &mut Editor, id: AgentId) {
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    if let Some(why) = forget_blocker(a) {
        ed.error(why);
        return;
    }
    let mut lines = vec![format!(
        "Removes {}'s record from the agent panel: its task, history, test results and comments.",
        a.name
    )];
    let blocked = a
        .worktree
        .as_ref()
        .and_then(|w| ed.agents.persist.blocked(&w.main_root));
    lines.push(match blocked {
        _ if super::persist::enabled(ed).is_none() => {
            "Agent records are not saved in this session (sessions are off), so this only removes it from the panel.".to_string()
        }
        Some(why) => format!(
            "Agent records of this repository are not saved in this session ({why}), so this only removes it from the panel; the records file is left as it is."
        ),
        None => "It is also removed from the saved agent records, so it does not come back after a restart.".to_string(),
    });
    if let Some(w) = &a.worktree {
        lines.push(format!(
            "Its worktree {} was removed when it was archived; nothing on disk is deleted now.",
            w.path.display()
        ));
        lines.push(format!(
            "The branch {} stays, with its commits; delete it with git if you no longer want it.",
            w.branch
        ));
    }
    ed.agents.panel.overlay = Some(Overlay::Confirm(Confirm {
        title: format!("forget {}", a.name),
        lines,
        keys: vec![
            ("y".into(), "forget the record".into()),
            ("n".into(), "cancel".into()),
        ],
        action: ConfirmAction::Forget(id),
    }));
}

/// Removes an archived agent's record from the panel and, when records are
/// saved, from the records file at the next save. Nothing on disk but the
/// records file changes: the worktree is gone already and the branch stays.
pub fn forget(ed: &mut Editor, id: AgentId) {
    let Some(i) = ed.agents.agents.iter().position(|a| a.id == id) else {
        return;
    };
    if let Some(why) = forget_blocker(&ed.agents.agents[i]) {
        ed.error(why);
        return;
    }
    let mut a = ed.agents.agents.remove(i);
    if a.is_running() || a.tests_running() {
        a.stop();
    }
    if let Some(w) = &a.worktree {
        super::persist::forget(ed, w);
    }
    let p = &mut ed.agents.panel;
    p.selected = p.selected.min(ed.agents.agents.len().saturating_sub(1));
    p.scroll = 0;
    if p.terminal_focus == Some(id) {
        p.terminal_focus = None;
    }
    super::conflicts::refresh(ed);
    ed.info(match &a.worktree {
        Some(w) => format!("forgot {}'s record; branch {} kept", a.name, w.branch),
        None => format!("forgot {}'s record", a.name),
    });
}
