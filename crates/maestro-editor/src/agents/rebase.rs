//! Rebasing an agent's own work onto the branch `I` integrates into, for
//! an agent whose starting commit is not on that branch.
//!
//! `I` refuses such an agent: integrating would also bring in the commits
//! its base has and the target lacks. When it refuses, it names those
//! commits and, if [`blocker`] allows, offers this: the agent's commits
//! since its base (and its uncommitted work, committed first) are moved
//! onto the target's `HEAD`, as `git rebase --onto <target> <base>` in its
//! worktree. The branch's old tip is kept as a branch named
//! `<branch>-before-rebase`. Afterward the agent's recorded base is the
//! target's `HEAD`, so `I` integrates only the agent's work.

use std::path::PathBuf;

use maestro_git::{RebaseCheck, RebaseError, RebaseOnto};

use super::actions::{
    OffTarget, busy, commit_list, found_refusal, list_paths, open_agent_worktree, open_exact,
    unsaved_under,
};
use super::panel::{Confirm, ConfirmAction, Overlay};
use super::{AgentId, TestOutput, WorktreeInfo};
use crate::editor::Editor;

/// What the rebase will do, gathered before the dialog opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseFacts {
    pub name: String,
    pub worktree: WorktreeInfo,
    /// The main worktree's branch.
    pub target: String,
    /// Its `HEAD` when checked: the commit the work moves onto.
    pub onto: String,
    /// The agent's commits since its base, newest first.
    pub moving: maestro_git::Commits,
    /// Uncommitted and untracked files, without the test run's: committed
    /// first, with `message`.
    pub uncommitted: Vec<PathBuf>,
    pub message: String,
    /// Files Maestro's test run created, still as it left them. They are
    /// not committed and stay untracked in the worktree.
    pub left_behind: Vec<PathBuf>,
    /// The agent's records of those files when checked.
    pub outputs_checked: Vec<TestOutput>,
    /// The branch's commit and the tree of every file when checked.
    pub checked_head: String,
    pub checked_tree: String,
    /// The branch the old tip is kept as.
    pub keep_as: String,
    /// Why the conflict dry run could not run, if it could not.
    pub unchecked: Option<String>,
}

/// Why the rebase is refused for the agent, judged from the record. The
/// rebase offer is made only when this is `None`, and [`rebase`] checks it
/// first, so the dialog never offers a rebase that this refuses.
pub(crate) fn blocker(a: &super::Agent) -> Option<String> {
    if let Some(why) = found_refusal(a) {
        return Some(why);
    }
    if a.worktree.is_none() {
        return Some("the agent has no worktree yet".into());
    }
    if a.archived {
        return Some("the agent's worktree was archived".into());
    }
    if a.base_status().is_decided() {
        return Some(format!("{} is {}", a.name, a.base_status().label()));
    }
    if let Some(note) = &a.worktree_note {
        return Some(note.clone());
    }
    if a.is_running() {
        return Some(format!(
            "{} is still running and could change its branch during the rebase; wait for it to \
             finish or end it first",
            a.name
        ));
    }
    if a.tests_running() {
        return Some("tests are running for this agent; wait for them to finish".into());
    }
    if !a.parts.is_empty() {
        return Some(
            "parts of its work were already integrated from its starting commit; the rest goes \
             in with I or after rejecting it"
                .into(),
        );
    }
    busy(a)
}

/// The free branch name for the old tip: `<branch>-before-rebase`, then
/// `-2`, `-3` and on.
fn free_name(repo: &maestro_git::Repository, branch: &str) -> Result<String, String> {
    for n in 1..100 {
        let name = if n == 1 {
            format!("{branch}-before-rebase")
        } else {
            format!("{branch}-before-rebase-{n}")
        };
        if !repo.branch_exists(&name).map_err(|e| e.to_string())? {
            return Ok(name);
        }
    }
    Err(format!(
        "no free branch name for {branch}-before-rebase-<n>"
    ))
}

/// Checks what a rebase onto the target would do, on a background thread.
/// `Err` says why it is not offered: the worktree is gone, detached or in
/// the middle of an operation, the work would conflict, or there is no
/// work to move.
pub(crate) fn offer(
    git: &maestro_git::Git,
    w: &WorktreeInfo,
    off: &OffTarget,
    outputs: &[TestOutput],
    message: &str,
) -> Result<RebaseFacts, String> {
    let wt = open_agent_worktree(git, w)?;
    let main = open_exact(git, &w.main_root)?;
    let onto = main.resolve("HEAD").map_err(|e| e.to_string())?;
    if let Some(op) = wt.operation().map_err(|e| e.to_string())? {
        return Err(format!(
            "a {} is in progress in the agent's worktree ({})",
            op.label(),
            op.way_out()
        ));
    }
    let made = super::test_outputs::still_made_by_tests(&wt, outputs)?;
    let snap = wt
        .work_snapshot_leaving_out(&made)
        .map_err(|e| format!("could not read the agent's files: {e}"))?;
    let st = wt.status().map_err(|e| e.to_string())?;
    let uncommitted: Vec<PathBuf> = st
        .uncommitted()
        .map(|e| e.path.clone())
        .chain(
            st.untracked()
                .map(|e| e.path.clone())
                .filter(|p| !made.contains(p)),
        )
        .collect();
    let moving = wt
        .commits_between(&w.base, &snap.head, super::actions::COMMITS_SHOWN)
        .map_err(|e| e.to_string())?;
    if moving.total == 0 && uncommitted.is_empty() {
        return Err("the agent has no work of its own to move".into());
    }
    let extra = (!uncommitted.is_empty()).then_some(snap.commit.as_str());
    let unchecked = match wt.rebase_check(&onto, &w.base, &snap.head, extra) {
        Ok(RebaseCheck::Clean { .. }) => None,
        Ok(RebaseCheck::Conflicts(files)) => {
            return Err(format!(
                "moving the agent's work onto {} would conflict in {}",
                off.target,
                list_paths(&files)
            ));
        }
        Ok(RebaseCheck::MergeCommits(m)) => {
            return Err(format!(
                "the agent's branch has {} merge commit(s), which a rebase would flatten",
                m.len()
            ));
        }
        Err(e) => Some(e.to_string()),
    };
    Ok(RebaseFacts {
        name: String::new(),
        worktree: w.clone(),
        target: off.target.clone(),
        keep_as: free_name(&wt, &w.branch)?,
        onto,
        moving,
        uncommitted,
        message: message.to_string(),
        left_behind: made,
        outputs_checked: outputs.to_vec(),
        checked_head: snap.head,
        checked_tree: snap.tree,
        unchecked,
    })
}

/// The check came back: say why `I` refused, and offer the rebase when it
/// can run.
pub(crate) fn offered(
    ed: &mut Editor,
    id: AgentId,
    off: &OffTarget,
    offer: Result<RebaseFacts, String>,
) {
    let refused = off.words();
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    a.log(format!("integrate refused: {refused}"));
    // The record's reason comes first: it is what the user can act on.
    let offer = match blocker(a) {
        Some(why) => Err(why),
        None => offer,
    };
    let name = a.name.clone();
    let mut msg = format!("not integrated, nothing changed: {refused}");
    match offer {
        Err(why) => {
            msg.push_str(&format!(
                ". Rebasing its work onto {} is not offered: {why}",
                off.target
            ));
            ed.error(msg);
        }
        Ok(mut facts) => {
            facts.name = name;
            ed.error(msg);
            ed.agents.panel.overlay = Some(Overlay::Confirm(dialog(id, &refused, facts)));
        }
    }
}

/// The confirmation: what is wrong, and exactly what `y` does.
pub fn dialog(agent: AgentId, refused: &str, f: RebaseFacts) -> Confirm {
    let w = &f.worktree;
    let short = super::create::short;
    let mut lines = vec![format!("I refused: {refused}.")];
    if !f.uncommitted.is_empty() {
        lines.push(format!(
            "1. Commits the uncommitted work in {} on {} first, as I would, with the subject \"{}\".",
            list_paths(&f.uncommitted),
            w.branch,
            super::task_summary(&f.message)
        ));
    }
    let what = match (f.moving.total, f.uncommitted.is_empty()) {
        (0, _) => "that commit".to_string(),
        (n, true) => format!("its {n} commit(s) ({})", commit_list(&f.moving)),
        (n, false) => format!(
            "its {n} commit(s) ({}) and that commit",
            commit_list(&f.moving)
        ),
    };
    lines.push(format!(
        "{}Moves {what} onto {} at {}, as git rebase --onto {} {} {} in the agent's worktree. \
         The commits named above, which {} lacks, are left out.",
        if f.uncommitted.is_empty() { "" } else { "2. " },
        f.target,
        short(&f.onto),
        f.target,
        short(&w.base),
        w.branch,
        f.target
    ));
    lines.push(format!(
        "{} is rewritten. Its tip before the rebase is kept as the branch {}; \
         git reset --hard {} in the worktree puts it back.",
        w.branch, f.keep_as, f.keep_as
    ));
    lines.push(format!(
        "Afterward the agent starts from {} and the review refreshes; then I integrates only \
         its work. Nothing goes into {} now.",
        short(&f.onto),
        f.target
    ));
    match &f.unchecked {
        Some(e) => lines.push(format!(
            "Maestro could not check for conflicts first ({e}). If git stops on one, the rebase \
             is aborted and the branch is put back as it was."
        )),
        None => lines.push(
            "A dry run found no conflicts. If git stops on one anyway, the rebase is aborted and \
             the branch is put back as it was."
                .into(),
        ),
    }
    if !f.left_behind.is_empty() {
        lines.push(format!(
            "Files Maestro's test run created stay untracked and are not moved: {}.",
            list_paths(&f.left_behind)
        ));
    }
    Confirm {
        title: format!("rebase {} onto {}", f.name, f.target),
        lines,
        keys: vec![
            ("y".into(), format!("rebase onto {}", f.target)),
            ("n".into(), "cancel".into()),
        ],
        action: ConfirmAction::Rebase {
            agent,
            facts: Box::new(f),
        },
    }
}

/// The result of a rebase attempt.
struct Rebased {
    /// Maestro's commit of the uncommitted work, when it was made and
    /// kept.
    committed: Option<String>,
    outcome: Result<maestro_git::RebaseOutcome, String>,
}

/// `y`: rebases on a background thread.
pub fn rebase(ed: &mut Editor, id: AgentId, facts: RebaseFacts) {
    let git = ed.git.git().clone();
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    let refuse =
        |ed: &mut Editor, why: String| ed.error(format!("not rebased, nothing changed: {why}"));
    if let Some(why) = blocker(a) {
        return refuse(ed, why);
    }
    if a.worktree.as_ref() != Some(&facts.worktree) {
        return refuse(
            ed,
            "the agent's worktree record changed; press I again".into(),
        );
    }
    if a.test_outputs != facts.outputs_checked {
        return refuse(
            ed,
            "the files Maestro's test run created changed after the dialog was checked; press I \
             again"
                .into(),
        );
    }
    let unsaved = unsaved_under(ed, &facts.worktree.path);
    if !unsaved.is_empty() {
        return refuse(
            ed,
            format!(
                "unsaved edits in {}; save or close them first",
                unsaved.join(", ")
            ),
        );
    }
    if let Some(a) = ed.agents.get_mut(id) {
        a.git_busy = Some("rebasing");
    }
    let spawned = ed.spawn_background("agent-rebase", move |tx| {
        let result = run(&git, &facts);
        tx.send(move |ed| rebased(ed, id, facts, result));
    });
    if let Err(e) = spawned {
        if let Some(a) = ed.agents.get_mut(id) {
            a.git_busy = None;
        }
        ed.error(format!("could not start the rebase: {e}"));
    }
}

fn run(git: &maestro_git::Git, f: &RebaseFacts) -> Rebased {
    let mut out = Rebased {
        committed: None,
        outcome: Err(String::new()),
    };
    match run_inner(git, f, &mut out.committed) {
        Ok(o) => out.outcome = Ok(o),
        Err(e) => out.outcome = Err(e),
    }
    out
}

fn run_inner(
    git: &maestro_git::Git,
    f: &RebaseFacts,
    committed: &mut Option<String>,
) -> Result<maestro_git::RebaseOutcome, String> {
    let w = &f.worktree;
    let again = "; nothing changed. Press I again to see what it would do now";
    let wt = open_agent_worktree(git, w).map_err(|e| format!("{e}{again}"))?;
    let main = open_exact(git, &w.main_root)?;
    match main.head().map_err(|e| e.to_string())? {
        maestro_git::Head::Branch(b) if b == f.target => {}
        other => {
            return Err(format!(
                "the main worktree is on {}, not {}{again}",
                other.label(),
                f.target
            ));
        }
    }
    if main.resolve("HEAD").map_err(|e| e.to_string())? != f.onto {
        return Err(format!(
            "{} moved after the dialog was checked{again}",
            f.target
        ));
    }
    let made = super::test_outputs::still_made_by_tests(&wt, &f.outputs_checked)?;
    if made != f.left_behind {
        return Err(format!(
            "files Maestro's test run created changed after the dialog was checked{again}"
        ));
    }
    let snap = wt
        .work_snapshot_leaving_out(&made)
        .map_err(|e| format!("could not read the agent's files: {e}"))?;
    if snap.head != f.checked_head || snap.tree != f.checked_tree {
        return Err(format!(
            "the agent's branch or files changed after the dialog was checked{again}"
        ));
    }
    if !f.uncommitted.is_empty() {
        let c = wt
            .commit_all_leaving_out(&f.message, &made)
            .map_err(|e| format!("committing the agent's work failed: {e}"))?;
        *committed = c.clone();
        let Some(c) = c else {
            return Err(format!("there was nothing to commit after all{again}"));
        };
        // The commit must hold exactly the files the dialog was checked on.
        let shape = wt.commit_shape(&c).map_err(|e| e.to_string())?;
        if shape.parents != [f.checked_head.as_str()] || shape.tree != f.checked_tree {
            return Err(format!(
                "the agent's files changed while Maestro committed them; nothing was rebased \
                 (the commit {} stays on {})",
                super::create::short(&c),
                w.branch
            ));
        }
    }
    let tip = committed.clone().unwrap_or_else(|| f.checked_head.clone());
    let r = wt.rebase_onto(RebaseOnto {
        branch: &w.branch,
        upstream: &w.base,
        onto: &f.onto,
        tip: &tip,
        keep_old_tip_as: &f.keep_as,
        undo_to: committed.as_ref().map(|_| f.checked_head.as_str()),
    });
    let undone = committed.is_some();
    let put_back = if undone {
        format!(
            "; {} is back where it was and its uncommitted work is staged, not committed",
            w.branch
        )
    } else {
        format!("; {} is back where it was", w.branch)
    };
    match r {
        Ok(o) => Ok(o),
        Err(e) => {
            if !matches!(e, RebaseError::Refused(_) | RebaseError::AbortFailed { .. }) && undone {
                *committed = None;
            }
            Err(match e {
                RebaseError::Refused(why) => format!("{why}; nothing was rebased"),
                RebaseError::Conflicts(files) => format!(
                    "git stopped on a conflict in {}, so the rebase was aborted{put_back}",
                    list_paths(&files)
                ),
                RebaseError::Git(g) => {
                    format!("git failed ({g}); the rebase was aborted{put_back}")
                }
                RebaseError::AbortFailed { cause, detail } => format!(
                    "{cause}, and putting the branch back failed: {detail}. Look at the agent's \
                     worktree with git status before doing anything else"
                ),
            })
        }
    }
}

fn rebased(ed: &mut Editor, id: AgentId, f: RebaseFacts, r: Rebased) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    a.git_busy = None;
    let short = super::create::short;
    if let Some(c) = &r.committed {
        a.log(format!(
            "committed the worktree's work as {} before rebasing",
            short(c)
        ));
    }
    match r.outcome {
        Ok(o) => {
            let old_base = f.worktree.base.clone();
            if let Some(w) = a.worktree.as_mut() {
                w.base = f.onto.clone();
            }
            // Marks name hunks of the old review; the new one is against
            // the new base.
            a.marks.clear();
            let msg = format!(
                "rebased {} onto {} at {}: {} commit(s) moved, starting commit {} is now {}; \
                 the old tip is kept as branch {}. I integrates it now",
                a.name,
                f.target,
                short(&f.onto),
                o.commits.len(),
                short(&old_base),
                short(&f.onto),
                f.keep_as
            );
            a.log(msg.clone());
            ed.info(msg);
        }
        Err(e) => {
            let mut msg = format!("not rebased: {e}");
            if let Some(c) = &r.committed {
                msg.push_str(&format!(
                    " (the agent's work was committed on its branch as {})",
                    short(c)
                ));
            }
            a.log(msg.clone());
            ed.error(msg);
        }
    }
    super::review::request(ed, id);
    super::conflicts::refresh(ed);
}
