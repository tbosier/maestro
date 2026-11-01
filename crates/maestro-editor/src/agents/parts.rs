//! Integrating only the hunks marked in an agent's review.
//!
//! The marked hunks are applied to the agent's starting commit in memory
//! ([`maestro_git::Repository::tree_from_hunks`]), committed on a new
//! branch `maestro/<agent>-part-<n>`, and that commit goes through the
//! same pinned integration as a whole agent ([`super::actions::land`]):
//! the same checks of both worktrees, the target and the base, the same
//! contract checks before and after, and the same refusal of a dirty main
//! worktree. Neither worktree is touched; the agent keeps its work, so the
//! rest can be integrated or rejected later.

use std::collections::BTreeSet;
use std::path::PathBuf;

use maestro_git::{Head, HunkPick, HunksError, Integration};

use super::actions::{
    IntegrateFacts, Integrated, check_before_commit, how_label, integrate_dialog, land, list_paths,
    open_exact,
};
use super::marks::{HunkKey, Part};
use super::panel::Overlay;
use super::{AgentId, create::short};
use crate::editor::Editor;

/// What a part integration will commit and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartPlan {
    /// 1 for the agent's first part.
    pub n: usize,
    /// The new branch the part's commit goes on.
    pub branch: String,
    /// The marked hunks, in diff order.
    pub picks: Vec<HunkPick>,
    /// The tree the picks gave when the dialog was checked.
    pub tree: String,
    /// The agent's own branch, where the rest of its work stays.
    pub agent_branch: String,
}

impl PartPlan {
    /// What the review shows as integrated afterward.
    fn keys(&self) -> Vec<(HunkKey, Option<BTreeSet<usize>>)> {
        self.picks
            .iter()
            .map(|p| {
                (
                    HunkKey::from_parts(p.old_path.clone(), p.new_path.clone(), &p.hunk),
                    p.lines.clone(),
                )
            })
            .collect()
    }
}

/// Plain words for hunks that do not apply.
pub fn hunks_words(e: &HunksError, picks: &[HunkPick]) -> String {
    match e {
        HunksError::DoNotApply(failures) => {
            let each: Vec<String> = failures
                .iter()
                .map(|f| {
                    let label = picks
                        .get(f.pick)
                        .map(|p| format!("{} from {}", p.label(), p.source))
                        .unwrap_or_default();
                    format!("{label}: {}", f.reason)
                })
                .collect();
            format!("these hunks do not apply: {}", each.join("; "))
        }
        HunksError::Git(g) => g.to_string(),
    }
}

/// `I` with marks: checks the marked hunks on a background thread, then
/// asks with the integrate dialog for only them.
pub fn prepare(ed: &mut Editor, id: AgentId) {
    let git = ed.git.git().clone();
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    let Some(w) = a.worktree.clone() else {
        return;
    };
    let Some(review_head) = a.review.head.clone() else {
        ed.error("the review the marks are on has no commit yet; r refreshes it");
        return;
    };
    let picks = super::marks::picks(a);
    let first_n = a.parts.iter().map(|p| p.n).max().unwrap_or(0) + 1;
    let slug = super::slug(&a.name);
    let name = a.name.clone();
    let summary = super::task_summary(&a.task);
    let task = a.task.trim().to_string();
    let provider = a.provider.name.clone();
    let contract = a.contract.clone();
    let agent_running = a.is_running();
    let spawned = ed.spawn_background("agent-part-check", move |tx| {
        let facts = (|| -> Result<IntegrateFacts, String> {
            let main = open_exact(&git, &w.main_root)?;
            let Head::Branch(target) = main.head().map_err(|e| e.to_string())? else {
                return Err("the main worktree is not on a branch (detached HEAD)".into());
            };
            let mut facts = IntegrateFacts {
                name: name.clone(),
                branch: w.branch.clone(),
                worktree: w.path.clone(),
                main_root: w.main_root.clone(),
                base: w.base.clone(),
                target,
                uncommitted: Vec::new(),
                message: String::new(),
                agent_running,
                contract,
                out_of_scope: Vec::new(),
                verification: Vec::new(),
                conflicts_after: Vec::new(),
                clean_after: Vec::new(),
                unchecked: Vec::new(),
                own_conflicts: Vec::new(),
                checked_tree: String::new(),
                checked_head: review_head.clone(),
                part: None,
            };
            let (_wt, main) = check_before_commit(&git, &facts)?;
            // The hunks were computed against the fork point of the
            // agent's branch; it must be the starting commit itself.
            if !main
                .can_fast_forward(&w.base, &review_head)
                .map_err(|e| e.to_string())?
            {
                return Err(format!(
                    "the review the marks come from is on a commit that does not descend from \
                     the agent's starting commit {}; r refreshes the review",
                    short(&w.base)
                ));
            }
            let tree = main
                .tree_from_hunks(&w.base, &picks)
                .map_err(|e| hunks_words(&e, &picks))?;
            let mut n = first_n;
            let branch = loop {
                let b = format!("maestro/{slug}-part-{n}");
                if !main.branch_exists(&b).map_err(|e| e.to_string())? {
                    break b;
                }
                n += 1;
                if n > first_n + 1000 {
                    return Err(format!("no free branch name for maestro/{slug}-part-<n>"));
                }
            };
            let mut message = format!(
                "{summary} (part {n})\n\nOnly these hunks of {name}'s work; the rest stays on \
                 {}:\n",
                w.branch
            );
            for p in &picks {
                message.push_str(&format!("- {}\n", p.label()));
            }
            message.push_str(&format!(
                "\nTask: {task}\nMaestro agent: {name} ({provider}), part {n}"
            ));
            facts.message = message;
            // Whether the part conflicts with the target now.
            let dry = main
                .commit_tree(&tree, &[&w.base], "maestro part dry run")
                .map_err(|e| e.to_string())?;
            let head = main.resolve("HEAD").map_err(|e| e.to_string())?;
            if let Ok(maestro_git::MergeCheck::Conflicts { files, .. }) =
                main.merge_check(&head, &dry)
            {
                facts.own_conflicts = files;
            }
            if let Some(c) = &facts.contract {
                let mut seen: Vec<PathBuf> = Vec::new();
                for p in &picks {
                    for path in [&p.old_path, &p.new_path].into_iter().flatten() {
                        if seen.contains(path) {
                            continue;
                        }
                        seen.push(path.clone());
                        if let Some(why) = c.violation(path) {
                            facts.out_of_scope.push((path.clone(), why));
                        }
                    }
                }
            }
            facts.verification.push(
                "Maestro has not tested these hunks on their own; t tests all of the agent's work."
                    .into(),
            );
            facts.checked_tree = tree.clone();
            facts.part = Some(Box::new(PartPlan {
                n,
                branch,
                picks,
                tree,
                agent_branch: w.branch.clone(),
            }));
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
                    a.log(format!("integrating the marked hunks refused: {e}"));
                }
                ed.error(format!("not integrated, nothing changed: {e}"));
            }
        });
    });
    if let Err(e) = spawned {
        ed.error(format!("could not start the integration check: {e}"));
    }
}

/// The dialog's steps for a part: every hunk, what stays, and how.
pub fn steps(f: &IntegrateFacts, p: &PartPlan, how: Integration) -> Vec<String> {
    let mut lines = vec![format!(
        "Only the {} marked hunk(s) of {}:",
        p.picks.len(),
        f.name
    )];
    // Every hunk is listed: a dialog too long for the window cannot be
    // accepted.
    for pick in &p.picks {
        lines.push(format!("  {}", pick.label()));
    }
    lines.push(format!(
        "The rest of the work stays on {} and in its worktree, which Maestro does not touch; it can be integrated or rejected later.",
        p.agent_branch
    ));
    let subject = f.message.lines().next().unwrap_or_default();
    lines.push(format!(
        "1. Commit exactly these hunks on the starting commit {} to the new branch {} as \"{subject}\".",
        short(&f.base),
        p.branch
    ));
    let step = match how {
        Integration::Merge => format!(
            "2. Merge {} into {} with a merge commit.",
            p.branch, f.target
        ),
        Integration::FastForwardOnly => format!(
            "2. Fast-forward {} to {} (refused if {} has moved on).",
            f.target, p.branch, f.target
        ),
        Integration::CherryPick => format!(
            "2. Copy the commit of {} onto {} (cherry-pick).",
            p.branch, f.target
        ),
    };
    lines.push(format!(
        "{step} The main worktree is {}.",
        f.main_root.display()
    ));
    lines.push(
        "Before step 1 Maestro checks both worktrees and the main worktree again, and stops on a change. Git then refuses on conflicts."
            .into(),
    );
    lines
}

/// Commits the marked hunks on a new branch and integrates that commit.
/// Runs on a background thread.
pub(crate) fn run(
    git: &maestro_git::Git,
    f: &IntegrateFacts,
    p: &PartPlan,
    how: Integration,
) -> Integrated {
    let mut out = Integrated::new(f);
    let (_wt, main) = match check_before_commit(git, f) {
        Ok(r) => r,
        Err(e) => {
            out.outcome = Err(format!("{e}; nothing changed"));
            return out;
        }
    };
    let tree = match main.tree_from_hunks(&f.base, &p.picks) {
        Ok(t) => t,
        Err(e) => {
            out.outcome = Err(format!("{}; nothing changed", hunks_words(&e, &p.picks)));
            return out;
        }
    };
    if tree != p.tree {
        out.outcome = Err(
            "the marked hunks give other files than when the dialog was checked; nothing \
             changed. Press I again"
                .into(),
        );
        return out;
    }
    match main.branch_exists(&p.branch) {
        Ok(false) => {}
        Ok(true) => {
            out.outcome = Err(format!(
                "the branch {} exists now; nothing changed. Press I again",
                p.branch
            ));
            return out;
        }
        Err(e) => {
            out.outcome = Err(format!("{e}; nothing changed"));
            return out;
        }
    }
    let commit = match main.commit_on(&tree, &f.base, &f.message) {
        Ok(c) => c,
        Err(e) => {
            out.outcome = Err(format!("committing the marked hunks failed: {e}"));
            return out;
        }
    };
    if let Err(e) = main.create_branch(&p.branch, &commit) {
        out.outcome = Err(format!(
            "could not create {}: {e}; nothing changed",
            p.branch
        ));
        return out;
    }
    out.committed = Some(commit.clone());
    land(&main, &main, f, &p.branch, &commit, how, &mut out);
    out
}

/// The part's integration finished (or failed).
pub(crate) fn integrated(
    ed: &mut Editor,
    id: AgentId,
    how: Integration,
    plan: PartPlan,
    r: Integrated,
) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    a.git_busy = None;
    match r.outcome {
        Ok(o) => {
            let keys = plan.keys();
            a.marks.retain(|m| !keys.iter().any(|(k, _)| *k == m.key));
            a.parts.push(Part {
                n: plan.n,
                branch: plan.branch.clone(),
                commit: r.committed.clone().unwrap_or_default(),
                hunks: keys,
            });
            let labels: Vec<String> = plan.picks.iter().map(|p| p.label()).collect();
            a.log(format!(
                "integrated part {} ({} hunk(s): {}) into {} by {} from {}: now at {}; the rest \
                 stays in the worktree",
                plan.n,
                plan.picks.len(),
                labels.join(", "),
                r.target,
                how_label(how),
                plan.branch,
                short(&o.new_head)
            ));
            if !r.accepted.is_empty() {
                a.log(format!(
                    "integrated with files outside the task contract, after O: {}",
                    list_paths(&r.accepted)
                ));
            }
            let msg = format!(
                "integrated part {} of {} ({} hunk(s)) into {} by {}: now at {}; the rest stays \
                 on {}",
                plan.n,
                a.name,
                plan.picks.len(),
                r.target,
                how_label(how),
                short(&o.new_head),
                plan.agent_branch
            );
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
                ed.info(msg);
            }
        }
        Err(e) => {
            let mut msg = format!("not integrated: {e}");
            if let Some(c) = &r.committed {
                msg.push_str(&format!(
                    " (the marked hunks were committed as {} on branch {}, which stays)",
                    short(c),
                    plan.branch
                ));
            }
            a.log(msg.clone());
            ed.error(msg);
        }
    }
    super::review::request(ed, id);
    super::conflicts::refresh(ed);
}
