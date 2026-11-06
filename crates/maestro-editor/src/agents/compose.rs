//! Composing one result from hunks of several agents of one task.
//!
//! `C` on an agent opens the compose view for its task group (the agents
//! started on one task with `N`). It lists the files any of them changed
//! and, for the file chosen, every agent's hunks side by side, one column
//! per agent with its name and its last test result. Hunks that cover the
//! same lines of the starting commit sit in the same row band, so the
//! alternatives line up.
//!
//! `space` marks hunks, from any of the agents; `ret` checks on a
//! background thread that the marked hunks apply together on the agents'
//! common starting commit ([`maestro_git::Repository::tree_from_hunks`]),
//! naming each conflict by hunk and agent. When they do, a dialog lists
//! them, and `y` commits exactly them on that commit, on a new branch
//! `maestro/compose-<id>` with a worktree of its own. The result shows in
//! the panel as an entry of origin [`Origin::Composed`] with the group's
//! task and contract: it is tested, reviewed and integrated like any
//! agent's work, through the same pinned integration. The record says
//! which hunk came from which agent, and so does the commit message.

use std::path::PathBuf;

use maestro_git::{DiffFile, DiffLineKind, HunkPick, HunksError};

use super::marks::{HunkKey, Mark, unmarkable};
use super::panel::{Confirm, ConfirmAction, Overlay, agent_layout};
use super::{Agent, AgentId, Origin, ProviderKind, ProviderSpec, Status, WorktreeInfo};
use crate::editor::Editor;
use crate::key::{KeyCode, KeyEvent};

/// Where a composed result's hunks came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composition {
    pub sources: Vec<ComposedHunk>,
    /// The composed commit; empty until it is made.
    pub commit: String,
}

/// One hunk of a composed result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedHunk {
    /// The agent's name.
    pub agent: String,
    /// The hunk: file and line ranges ([`HunkPick::label`]).
    pub label: String,
    /// The agent's last test result when it was composed.
    pub test: String,
}

/// The compose view's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeView {
    pub group: u64,
    /// The columns, in panel order.
    pub agents: Vec<AgentId>,
    /// Index into [`files`].
    pub file: usize,
    /// Index into [`items`] of the current file.
    pub cursor: usize,
    /// First row of the grid shown.
    pub top: usize,
    pub marks: Vec<(AgentId, Mark)>,
    /// The last check's refusals: agent, hunk and why.
    pub problems: Vec<(AgentId, HunkKey, String)>,
    /// A check runs.
    pub checking: bool,
}

/// What `y` in the compose dialog makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposePlan {
    pub group: u64,
    pub task: String,
    pub contract: Option<super::contract::Contract>,
    pub base: String,
    pub main_root: PathBuf,
    /// The hunks, with the agents' names as their sources.
    pub picks: Vec<HunkPick>,
    pub sources: Vec<ComposedHunk>,
    /// The tree they gave when checked.
    pub tree: String,
}

/// One hunk of one agent in the current file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub agent: AgentId,
    /// Column.
    pub col: usize,
    /// Index into the agent's review files.
    pub file: usize,
    pub hunk: usize,
    /// Hunks that cover the same base lines share a band.
    pub band: usize,
}

/// A cell of the side-by-side grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    /// A hunk's header line.
    Head {
        item: usize,
        header: String,
        marked: bool,
        /// Why the last check refused it.
        problem: Option<String>,
    },
    Line {
        kind: DiffLineKind,
        text: String,
    },
    Note(String),
}

/// Rows of cells, one per column, and the row each item's header is on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grid {
    pub rows: Vec<Vec<Option<Cell>>>,
    pub item_rows: Vec<usize>,
}

/// The grid rows the content area shows (one row is the column heads).
pub fn grid_height(area: crate::layout::Rect) -> usize {
    (agent_layout(area).content.height as usize)
        .saturating_sub(1)
        .max(1)
}

/// `tests ok 3`, `tests FAIL 1/4`, `no tests run`.
pub fn test_label(a: &Agent) -> String {
    match a.latest_test() {
        Some(t) => format!("tests {}", t.short()),
        None => "no tests run".into(),
    }
}

/// The agents that take part in composing for `group`: records with a
/// worktree, not archived, not decided, not composed results themselves.
fn members(ed: &Editor, group: u64) -> Vec<AgentId> {
    ed.agents
        .agents
        .iter()
        .filter(|a| a.group == group)
        .filter(|a| !matches!(a.origin, Origin::Found | Origin::Composed))
        .filter(|a| a.worktree.is_some() && !a.archived && !a.base_status().is_decided())
        .map(|a| a.id)
        .collect()
}

/// `C` in the panel: the compose view for the selected agent's task.
pub fn open(ed: &mut Editor, id: AgentId) {
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    if a.origin == Origin::Found {
        ed.error("a found worktree has no task to compose for; a adopts it first");
        return;
    }
    let group = a.group;
    let ids = members(ed, group);
    if ids.len() < 2 {
        ed.error(
            "composing needs two or more agents on this task that are not rejected, integrated \
             or archived; N starts another agent on it",
        );
        return;
    }
    let worktrees: Vec<WorktreeInfo> = ids
        .iter()
        .filter_map(|i| ed.agents.get(*i).and_then(|a| a.worktree.clone()))
        .collect();
    if worktrees
        .iter()
        .any(|w| w.base != worktrees[0].base || w.main_root != worktrees[0].main_root)
    {
        ed.error("these agents started from different commits; Maestro composes only on one");
        return;
    }
    for i in &ids {
        super::review::request(ed, *i);
    }
    ed.agents.panel.compose = Some(ComposeView {
        group,
        agents: ids,
        file: 0,
        cursor: 0,
        top: 0,
        marks: Vec::new(),
        problems: Vec::new(),
        checking: false,
    });
}

/// Every path any column's agent changed, sorted.
pub fn files(ed: &Editor, v: &ComposeView) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = v
        .agents
        .iter()
        .filter_map(|i| ed.agents.get(*i))
        .flat_map(|a| a.review.files.iter().filter_map(|f| f.path().cloned()))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn file_of<'a>(a: &'a Agent, path: &PathBuf) -> Option<(usize, &'a DiffFile)> {
    a.review
        .files
        .iter()
        .enumerate()
        .find(|(_, f)| f.path() == Some(path))
}

/// The hunks of the current file, band by band, column by column.
pub fn items(ed: &Editor, v: &ComposeView) -> Vec<Item> {
    let Some(path) = files(ed, v).get(v.file).cloned() else {
        return Vec::new();
    };
    // (start, end, col, agent, file, hunk)
    let mut raw: Vec<(usize, usize, usize, AgentId, usize, usize)> = Vec::new();
    for (col, id) in v.agents.iter().enumerate() {
        let Some(a) = ed.agents.get(*id) else {
            continue;
        };
        // Every entry of the path: a file whose type changed is a deletion
        // and an addition of one path.
        for (fi, f) in a.review.files.iter().enumerate() {
            if f.path() != Some(&path) || unmarkable(f).is_some() {
                continue;
            }
            for (hi, h) in f.hunks.iter().enumerate() {
                raw.push((h.old_start, h.old_start + h.old_len, col, *id, fi, hi));
            }
        }
    }
    raw.sort_by_key(|r| (r.0, r.1, r.2));
    let mut out = Vec::new();
    let mut band = 0;
    let mut reach: Option<usize> = None;
    for (start, end, col, agent, file, hunk) in raw {
        match reach {
            Some(r) if start <= r => reach = Some(r.max(end)),
            Some(_) => {
                band += 1;
                reach = Some(end);
            }
            None => reach = Some(end),
        }
        out.push(Item {
            agent,
            col,
            file,
            hunk,
            band,
        });
    }
    out.sort_by_key(|i| (i.band, i.col));
    out
}

fn key_of(ed: &Editor, it: &Item) -> Option<(HunkKey, DiffFile)> {
    let a = ed.agents.get(it.agent)?;
    let f = a.review.files.get(it.file)?;
    let h = f.hunks.get(it.hunk)?;
    Some((HunkKey::of(f, h), f.clone()))
}

/// The side-by-side grid of the current file.
pub fn grid(ed: &Editor, v: &ComposeView) -> Grid {
    let items = items(ed, v);
    let ncols = v.agents.len();
    let mut g = Grid {
        rows: Vec::new(),
        item_rows: vec![0; items.len()],
    };
    // A note for columns whose agent did not change the file, or changed
    // it in a way that has no hunks to take.
    if let Some(path) = files(ed, v).get(v.file) {
        let notes: Vec<Option<Cell>> = v
            .agents
            .iter()
            .map(|id| {
                let a = ed.agents.get(*id)?;
                match file_of(a, path) {
                    None => Some(Cell::Note("no change to this file".into())),
                    Some((_, f)) => unmarkable(f).map(|why| Cell::Note(why.into())),
                }
            })
            .collect();
        if notes.iter().any(Option::is_some) {
            g.rows.push(notes);
            g.rows.push(vec![None; ncols]);
        }
    }
    let nbands = items.iter().map(|i| i.band + 1).max().unwrap_or(0);
    for band in 0..nbands {
        let start = g.rows.len();
        let mut cols: Vec<Vec<Cell>> = vec![Vec::new(); ncols];
        for (n, it) in items.iter().enumerate().filter(|(_, i)| i.band == band) {
            let Some(a) = ed.agents.get(it.agent) else {
                continue;
            };
            let Some(f) = a.review.files.get(it.file) else {
                continue;
            };
            let Some(h) = f.hunks.get(it.hunk) else {
                continue;
            };
            let key = HunkKey::of(f, h);
            g.item_rows[n] = start + cols[it.col].len();
            cols[it.col].push(Cell::Head {
                item: n,
                header: format!(
                    "@@ -{},{} +{},{} @@",
                    h.old_start, h.old_len, h.new_start, h.new_len
                ),
                marked: v.marks.iter().any(|(i, m)| *i == it.agent && m.key == key),
                problem: v
                    .problems
                    .iter()
                    .find(|(i, k, _)| *i == it.agent && *k == key)
                    .map(|(_, _, why)| why.clone()),
            });
            for l in &h.lines {
                cols[it.col].push(Cell::Line {
                    kind: l.kind,
                    text: l.text.clone(),
                });
            }
        }
        let height = cols.iter().map(Vec::len).max().unwrap_or(0);
        let mut iters: Vec<std::vec::IntoIter<Cell>> =
            cols.into_iter().map(Vec::into_iter).collect();
        for _ in 0..height {
            g.rows.push(iters.iter_mut().map(Iterator::next).collect());
        }
        if band + 1 < nbands {
            g.rows.push(vec![None; ncols]);
        }
    }
    g
}

pub fn handle_key(ed: &mut Editor, key: KeyEvent) {
    let Some(mut v) = ed.agents.panel.compose.clone() else {
        return;
    };
    let items = items(ed, &v);
    let nfiles = files(ed, &v).len();
    let ctrl = key.mods.ctrl;
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') if !ctrl => {
            ed.agents.panel.compose = None;
            return;
        }
        KeyCode::Char('?') => {
            super::panel::show_keys(ed);
            return;
        }
        // Down and up the column the cursor is in: items are ordered band
        // by band, so the next item may be another agent's.
        KeyCode::Char('j') | KeyCode::Down if !ctrl => {
            if let Some(col) = items.get(v.cursor).map(|i| i.col)
                && let Some(n) = (v.cursor + 1..items.len()).find(|&n| items[n].col == col)
            {
                v.cursor = n;
            }
        }
        KeyCode::Char('k') | KeyCode::Up if !ctrl => {
            if let Some(col) = items.get(v.cursor).map(|i| i.col)
                && let Some(n) = (0..v.cursor).rev().find(|&n| items[n].col == col)
            {
                v.cursor = n;
            }
        }
        KeyCode::Char('h' | 'l') | KeyCode::Left | KeyCode::Right if !ctrl => {
            let right = matches!(key.code, KeyCode::Char('l') | KeyCode::Right);
            if let Some(cur) = items.get(v.cursor) {
                // The nearest hunk in the next column over, in this band
                // when it has one there.
                let want = if right {
                    cur.col + 1
                } else {
                    cur.col.wrapping_sub(1)
                };
                let pick = items
                    .iter()
                    .enumerate()
                    .filter(|(_, i)| i.col == want)
                    .min_by_key(|(_, i)| i.band.abs_diff(cur.band))
                    .map(|(n, _)| n);
                if let Some(n) = pick {
                    v.cursor = n;
                }
            }
        }
        KeyCode::Tab if nfiles > 0 => {
            v.file = (v.file + 1) % nfiles;
            v.cursor = 0;
            v.top = 0;
        }
        KeyCode::BackTab if nfiles > 0 => {
            v.file = (v.file + nfiles - 1) % nfiles;
            v.cursor = 0;
            v.top = 0;
        }
        KeyCode::Char(' ') if !ctrl => match items.get(v.cursor) {
            Some(it) => toggle(ed, &mut v, it),
            None => ed.error("no hunk under the cursor"),
        },
        KeyCode::Char('r') if !ctrl => {
            for id in v.agents.clone() {
                super::review::request(ed, id);
            }
        }
        KeyCode::Enter => {
            ed.agents.panel.compose = Some(v);
            check(ed);
            return;
        }
        KeyCode::Char(':') if !ctrl => crate::prompt::open(ed, crate::prompt::PromptKind::Command),
        _ => {}
    }
    // Keep the cursor's hunk on screen.
    let g = grid(ed, &v);
    let height = grid_height(ed.area);
    if let Some(&row) = g.item_rows.get(v.cursor) {
        if row < v.top {
            v.top = row;
        } else if row >= v.top + height {
            v.top = row + 1 - height;
        }
    }
    if ed.agents.panel.compose.is_some() {
        ed.agents.panel.compose = Some(v);
    }
}

fn toggle(ed: &mut Editor, v: &mut ComposeView, it: &Item) {
    let Some((key, f)) = key_of(ed, it) else {
        return;
    };
    if let Some(i) = v
        .marks
        .iter()
        .position(|(a, m)| *a == it.agent && m.key == key)
    {
        v.marks.remove(i);
        return;
    }
    let Some(a) = ed.agents.get(it.agent) else {
        return;
    };
    if let super::marks::HunkState::Integrated { part, .. } =
        super::marks::hunk_state(a, &f, it.hunk)
    {
        ed.error(format!(
            "this hunk of {} was integrated in part {part}; it is already on the target branch",
            a.name
        ));
        return;
    }
    if let Some(pick) = HunkPick::new(a.name.clone(), &f, it.hunk, None) {
        v.marks.push((it.agent, Mark { key, pick }));
    }
}

/// `ret`: checks on a background thread that the marked hunks apply
/// together on the common starting commit, then asks.
fn check(ed: &mut Editor) {
    let git = ed.git.git().clone();
    let Some(v) = ed.agents.panel.compose.clone() else {
        return;
    };
    if v.checking {
        ed.error("a check is already running");
        return;
    }
    if v.marks.is_empty() {
        ed.error("no hunks marked: space marks the hunk under the cursor");
        return;
    }
    let Some(first) = v.agents.first().and_then(|i| ed.agents.get(*i)) else {
        return;
    };
    let Some(w) = first.worktree.clone() else {
        return;
    };
    let (task, contract, group) = (first.task.clone(), first.contract.clone(), first.group);
    // Each mark must still be in its agent's review.
    let mut picks: Vec<(AgentId, HunkPick, (usize, usize, usize))> = Vec::new();
    let mut stale = Vec::new();
    let mut heads = Vec::new();
    for (id, m) in &v.marks {
        let Some(a) = ed.agents.get(*id) else {
            stale.push(format!("{} (its agent is gone)", m.pick.label()));
            continue;
        };
        let at = a.review.files.iter().enumerate().find_map(|(fi, f)| {
            f.hunks
                .iter()
                .position(|h| HunkKey::of(f, h) == m.key)
                .map(|hi| (fi, hi))
        });
        match (at, &a.review.head) {
            (Some((fi, hi)), Some(head)) => {
                let col = v.agents.iter().position(|x| x == id).unwrap_or(0);
                picks.push((*id, m.pick.clone(), (fi, hi, col)));
                heads.push((a.name.clone(), head.clone()));
            }
            _ => stale.push(format!(
                "{} from {} (the agent changed it since it was marked)",
                m.pick.label(),
                a.name
            )),
        }
    }
    if !stale.is_empty() {
        ed.error(format!(
            "not composed: these marked hunks are no longer in their agents' work: {}; space \
             marks the current ones",
            stale.join("; ")
        ));
        return;
    }
    // In file order, then by place in the file, then by column.
    picks.sort_by(|a, b| {
        (a.1.path(), a.1.hunk.old_start, a.2.2).cmp(&(b.1.path(), b.1.hunk.old_start, b.2.2))
    });
    let sources: Vec<ComposedHunk> = picks
        .iter()
        .map(|(id, p, _)| ComposedHunk {
            agent: p.source.clone(),
            label: p.label(),
            test: ed.agents.get(*id).map(test_label).unwrap_or_default(),
        })
        .collect();
    let owners: Vec<(AgentId, HunkKey)> = picks
        .iter()
        .map(|(id, p, _)| (*id, HunkKey::of_pick(p)))
        .collect();
    let picks: Vec<HunkPick> = picks.into_iter().map(|(_, p, _)| p).collect();
    if let Some(c) = &mut ed.agents.panel.compose {
        c.checking = true;
        c.problems.clear();
    }
    let spawned = ed.spawn_background("agent-compose-check", move |tx| {
        let result = (|| -> Result<String, Result<Vec<(usize, String)>, String>> {
            let main = super::actions::open_exact(&git, &w.main_root).map_err(Err)?;
            for (name, head) in &heads {
                if !main
                    .can_fast_forward(&w.base, head)
                    .map_err(|e| Err(e.to_string()))?
                {
                    return Err(Err(format!(
                        "{name}'s review is on a commit that does not descend from the starting \
                         commit; r refreshes the reviews"
                    )));
                }
            }
            main.tree_from_hunks(&w.base, &picks).map_err(|e| match e {
                HunksError::DoNotApply(f) => {
                    Ok(f.into_iter().map(|f| (f.pick, f.reason)).collect())
                }
                HunksError::Git(g) => Err(g.to_string()),
            })
        })();
        tx.send(move |ed| {
            if let Some(c) = &mut ed.agents.panel.compose {
                c.checking = false;
            }
            match result {
                Ok(tree) => {
                    let plan = ComposePlan {
                        group,
                        task,
                        contract,
                        base: w.base.clone(),
                        main_root: w.main_root.clone(),
                        picks,
                        sources,
                        tree,
                    };
                    ed.agents.panel.overlay = Some(Overlay::Confirm(dialog(plan)));
                }
                Err(Ok(failures)) => {
                    let mut words = Vec::new();
                    let mut problems = Vec::new();
                    for (i, why) in failures {
                        if let (Some(p), Some((id, key))) = (picks.get(i), owners.get(i)) {
                            words.push(format!("{} from {}: {why}", p.label(), p.source));
                            problems.push((*id, key.clone(), why));
                        }
                    }
                    if let Some(c) = &mut ed.agents.panel.compose {
                        c.problems = problems;
                    }
                    ed.error(format!(
                        "not composed: the marked hunks do not apply together: {}",
                        words.join("; ")
                    ));
                }
                Err(Err(e)) => ed.error(format!("not composed: {e}")),
            }
        });
    });
    if let Err(e) = spawned {
        if let Some(c) = &mut ed.agents.panel.compose {
            c.checking = false;
        }
        ed.error(format!("could not start the check: {e}"));
    }
}

/// The dialog before composing: every hunk with its agent and test result.
pub fn dialog(plan: ComposePlan) -> Confirm {
    let short = super::create::short(&plan.base).to_string();
    let mut agents: Vec<&str> = plan.sources.iter().map(|s| s.agent.as_str()).collect();
    agents.dedup();
    let mut lines = vec![format!(
        "Compose one result from {} hunk(s) of {} on their common starting commit {short}; Maestro checked that they apply together:",
        plan.picks.len(),
        agents.join(", ")
    )];
    for s in &plan.sources {
        lines.push(format!("  {} from {} ({})", s.label, s.agent, s.test));
    }
    lines.push(format!(
        "y commits exactly these hunks on {short} to a new branch maestro/compose-<id> with a worktree of its own. The agents' worktrees and branches are not touched."
    ));
    lines.push(
        "It shows in the panel as a composed result with the same task and contract: t tests it, ret reviews it, I integrates it through the same checks as any agent. Nothing reaches the main worktree before that."
            .into(),
    );
    Confirm {
        title: format!("compose a result for task #{}", plan.group),
        lines,
        keys: vec![
            ("y".into(), "compose".into()),
            ("n".into(), "cancel".into()),
        ],
        action: ConfirmAction::Compose(Box::new(plan)),
    }
}

/// The provider a composed result is recorded with: no program runs.
pub fn provider() -> ProviderSpec {
    ProviderSpec {
        name: "composed".into(),
        kind: ProviderKind::Terminal,
        program: String::new(),
        args: Vec::new(),
        env: Vec::new(),
    }
}

/// `y` in the compose dialog: adds the composed entry and makes its commit,
/// branch and worktree on a background thread.
pub fn create(ed: &mut Editor, plan: ComposePlan) {
    if ed.agents.persist.loading {
        ed.error(
            "still reading the agent records saved for this repository; try again in a moment",
        );
        return;
    }
    let id = ed.agents.fresh_id();
    let mut a = Agent::new(id, plan.task.clone(), plan.group, provider());
    a.name = format!("compose-{}", id.0);
    a.origin = Origin::Composed;
    a.contract = plan.contract.clone();
    a.composition = Some(Composition {
        sources: plan.sources.clone(),
        commit: String::new(),
    });
    let from: Vec<String> = plan
        .sources
        .iter()
        .map(|s| format!("{} from {}", s.label, s.agent))
        .collect();
    a.log(format!("composing from: {}", from.join("; ")));
    a.git_busy = Some(super::CREATING);
    let name = a.name.clone();
    ed.agents.agents.push(a);
    ed.agents.panel.selected = ed.agents.agents.len() - 1;
    ed.agents.panel.compose = None;

    let short = super::create::short(&plan.base).to_string();
    let mut message = format!(
        "Compose: {}\n\nComposed by Maestro from hunks of agents on the same task, on {short}:\n",
        super::task_summary(&plan.task)
    );
    for s in &plan.sources {
        message.push_str(&format!("- {} from {} ({})\n", s.label, s.agent, s.test));
    }
    message.push_str(&format!(
        "\nTask: {}\nMaestro composed result: {name}",
        plan.task.trim()
    ));
    let git = ed.git.git().clone();
    let setting = ed.config.agents.worktree_dir.clone();
    let spawned = ed.spawn_background("agent-compose", move |tx| {
        let result = (|| -> Result<(WorktreeInfo, String), String> {
            let repo = super::actions::open_exact(&git, &plan.main_root)?;
            let tree = repo
                .tree_from_hunks(&plan.base, &plan.picks)
                .map_err(|e| super::parts::hunks_words(&e, &plan.picks))?;
            if tree != plan.tree {
                return Err("the hunks give other files than when they were checked".into());
            }
            let commit = repo
                .commit_on(&tree, &plan.base, &message)
                .map_err(|e| format!("committing the composed hunks failed: {e}"))?;
            let dir = super::create::worktree_root(&plan.main_root, setting.as_deref());
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
            for n in 0..100 {
                let leaf = if n == 0 {
                    format!("compose-{}", id.0)
                } else {
                    format!("compose-{}-{n}", id.0)
                };
                let branch = format!("maestro/{leaf}");
                let path = dir.join(&leaf);
                if path.exists() || repo.branch_exists(&branch).map_err(|e| e.to_string())? {
                    continue;
                }
                repo.add_worktree(&path, &branch, &commit)
                    .map_err(|e| format!("could not create the worktree: {e}"))?;
                let path = std::fs::canonicalize(&path).unwrap_or(path);
                return Ok((
                    WorktreeInfo {
                        path,
                        branch,
                        base: plan.base.clone(),
                        main_root: plan.main_root.clone(),
                    },
                    commit,
                ));
            }
            Err(format!("no free branch name for maestro/compose-{}", id.0))
        })();
        tx.send(move |ed| created(ed, id, result));
    });
    if let Err(e) = spawned {
        created(ed, id, Err(format!("could not start: {e}")));
    }
}

fn created(ed: &mut Editor, id: AgentId, result: Result<(WorktreeInfo, String), String>) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    a.git_busy = None;
    match result {
        Ok((info, commit)) => {
            a.log(format!(
                "composed as {} on branch {} in {}",
                super::create::short(&commit),
                info.branch,
                info.path.display()
            ));
            if let Some(c) = &mut a.composition {
                c.commit = commit;
            }
            let main_root = info.main_root.clone();
            let msg = format!(
                "composed {} on {}: t tests it, I integrates it",
                a.name, info.branch
            );
            a.worktree = Some(info);
            a.set_status(Status::ReadyForReview);
            ed.info(msg);
            super::persist::ensure_loaded(ed, &main_root);
            super::review::request(ed, id);
        }
        Err(e) => {
            let msg = format!("not composed: {e}");
            a.log(msg.clone());
            a.errors.push(msg.clone());
            a.set_status(Status::Failed);
            ed.error(msg);
        }
    }
}

pub fn hints() -> Vec<(&'static str, &'static str)> {
    vec![
        ("j/k", "hunk"),
        ("h/l", "agent"),
        ("tab", "file"),
        ("space", "mark"),
        ("ret", "check and compose"),
        ("r", "refresh"),
        ("S-tab", "previous file"),
        (":", "command"),
        ("esc", "back"),
    ]
}
