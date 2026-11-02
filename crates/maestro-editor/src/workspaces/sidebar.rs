//! The sidebar: agents grouped by what they need from you, then projects
//! and their worktrees. `space s` opens and focuses it; keys:
//!
//! | key | does |
//! |---|---|
//! | `j` `k` (arrows) | move |
//! | `g` `G` | first, last row |
//! | `ret` | project: fold or unfold; worktree or agent: switch to it |
//! | `l` `h` | unfold, fold (on a worktree, `h` folds its project) |
//! | `r` | scan again now |
//! | `q` | hide the sidebar |
//! | `esc` | back to the editor, sidebar stays |
//! | `space ...` | the normal `space` keys (`space s` hides it) |
//!
//! Rows are built from the last scan and the live agent records each time
//! they are drawn ([`rows`]); the selection is remembered by what it
//! points at, so a refresh does not move it.

use std::collections::HashSet;
use std::path::PathBuf;

use super::{AgentEntry, Attention, WorktreeRow};
use crate::editor::Editor;
use crate::key::{KeyCode, KeyEvent};

#[derive(Debug, Default)]
pub struct Sidebar {
    pub open: bool,
    /// Keys go to the sidebar.
    pub focused: bool,
    /// The selected row.
    pub selected: Option<RowKey>,
    /// Projects shown unfolded, by main worktree.
    pub expanded: HashSet<PathBuf>,
    /// Set by the front end when the screen is too narrow to draw the
    /// sidebar: it then takes no keys, so nothing acts on rows you cannot
    /// see.
    pub too_narrow: bool,
}

impl Sidebar {
    /// Open, shown and taking keys.
    pub fn has_focus(&self) -> bool {
        self.open && self.focused && !self.too_narrow
    }
}

/// What a row points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKey {
    Agent(String),
    Project(PathBuf),
    Worktree(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A group heading, with the number of rows under it when that means
    /// something.
    Heading(String),
    /// A line of information (loading, empty, an error).
    Note(String),
    Agent(AgentEntry),
    Project {
        root: PathBuf,
        name: String,
        expanded: bool,
        worktrees: usize,
        /// The editor is in one of its worktrees.
        current: bool,
        error: Option<String>,
    },
    Worktree {
        project: PathBuf,
        row: Box<WorktreeRow>,
        /// The editor is in this worktree.
        current: bool,
        /// Open buffers of this worktree with unsaved changes.
        unsaved: usize,
        /// The agent working in it, if any.
        agent: Option<AgentEntry>,
    },
}

impl Row {
    pub fn key(&self) -> Option<RowKey> {
        match self {
            Row::Agent(a) => Some(RowKey::Agent(a.key.clone())),
            Row::Project { root, .. } => Some(RowKey::Project(root.clone())),
            Row::Worktree { row, .. } => Some(RowKey::Worktree(row.path.clone())),
            Row::Heading(_) | Row::Note(_) => None,
        }
    }
}

/// The rows to draw, top to bottom.
pub fn rows(ed: &Editor) -> Vec<Row> {
    let ws = &ed.workspaces;
    let mut out = Vec::new();
    let agents = super::agent_entries(ed);
    if agents.is_empty() {
        out.push(Row::Heading("agents".into()));
        out.push(Row::Note("no agents (space a starts one)".into()));
    }
    let mut group: Option<&'static str> = None;
    for (i, a) in agents.iter().enumerate() {
        let g = a.attention.group();
        if group != Some(g) {
            group = Some(g);
            let n = agents[i..]
                .iter()
                .take_while(|b| b.attention.group() == g)
                .count();
            out.push(Row::Heading(format!("{g} ({n})")));
        }
        out.push(Row::Agent(a.clone()));
    }
    out.push(Row::Heading("projects".into()));
    if !ws.scanned {
        out.push(Row::Note(match &ws.scan_error {
            Some(e) => e.clone(),
            None => "scanning projects…".into(),
        }));
        return out;
    }
    if ws.projects.is_empty() {
        out.push(Row::Note(
            "no projects: add [projects] paths to the config".into(),
        ));
    }
    let current = super::current_root(ed);
    for p in &ws.projects {
        let expanded = ws.sidebar.expanded.contains(&p.root);
        out.push(Row::Project {
            root: p.root.clone(),
            name: p.name.clone(),
            expanded,
            worktrees: p.worktrees.len(),
            current: p.worktrees.iter().any(|w| w.path == current),
            error: p.error.clone(),
        });
        if !expanded {
            continue;
        }
        if let Some(e) = &p.error {
            out.push(Row::Note(e.clone()));
        }
        if let Some(n) = &p.records_note {
            out.push(Row::Note(format!("agent records: {n}")));
        }
        for w in &p.worktrees {
            let agent = agents
                .iter()
                .find(|a| a.worktree.as_deref() == Some(w.path.as_path()))
                .cloned();
            out.push(Row::Worktree {
                project: p.root.clone(),
                current: w.path == current,
                unsaved: super::unsaved_in(ed, &w.path),
                row: Box::new(w.clone()),
                agent,
            });
        }
    }
    out
}

/// The index of the selected row in `rows`: the remembered one, else the
/// first selectable row.
pub fn selected_index(ed: &Editor, rows: &[Row]) -> Option<usize> {
    let want = ed.workspaces.sidebar.selected.as_ref();
    want.and_then(|k| rows.iter().position(|r| r.key().as_ref() == Some(k)))
        .or_else(|| rows.iter().position(|r| r.key().is_some()))
}

/// Remembers a selection when there is none or its row is gone: the
/// current worktree's row, else the first row. Called when rows can
/// change (a scan, agent events), so a refresh never moves the
/// highlight under the cursor without it being stored.
pub(crate) fn pin_selection(ed: &mut Editor) {
    if !ed.workspaces.sidebar.open {
        return;
    }
    let rows = rows(ed);
    let present = ed
        .workspaces
        .sidebar
        .selected
        .as_ref()
        .is_some_and(|k| rows.iter().any(|r| r.key().as_ref() == Some(k)));
    if present {
        return;
    }
    let current = RowKey::Worktree(super::current_root(ed));
    let pick = rows
        .iter()
        .filter_map(Row::key)
        .find(|k| *k == current)
        .or_else(|| rows.iter().find_map(Row::key));
    ed.workspaces.sidebar.selected = pick;
}

/// Handles a key while the sidebar has focus. Returns false for keys that
/// belong to the normal keymap (`space`).
pub fn handle_key(ed: &mut Editor, key: KeyEvent) -> bool {
    if key == KeyEvent::char(' ') {
        return false;
    }
    let rows = rows(ed);
    let at = selected_index(ed, &rows);
    let selectable: Vec<usize> = (0..rows.len())
        .filter(|i| rows[*i].key().is_some())
        .collect();
    let select = |ed: &mut Editor, i: Option<usize>| {
        if let Some(i) = i {
            ed.workspaces.sidebar.selected = rows[i].key();
        }
    };
    let pos = at.and_then(|i| selectable.iter().position(|s| *s == i));
    match key.code {
        KeyCode::Esc => ed.workspaces.sidebar.focused = false,
        KeyCode::Char('q') => {
            ed.workspaces.sidebar.open = false;
            ed.workspaces.sidebar.focused = false;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let next = pos.map_or(0, |p| (p + 1).min(selectable.len().saturating_sub(1)));
            select(ed, selectable.get(next).copied());
        }
        KeyCode::Char('k') | KeyCode::Up => {
            let prev = pos.map_or(0, |p| p.saturating_sub(1));
            select(ed, selectable.get(prev).copied());
        }
        KeyCode::Char('g') | KeyCode::Home => select(ed, selectable.first().copied()),
        KeyCode::Char('G') | KeyCode::End => select(ed, selectable.last().copied()),
        KeyCode::Char('r') => {
            super::scan::request(ed);
            ed.info("scanning projects and worktrees");
        }
        KeyCode::Enter => match at.map(|i| &rows[i]) {
            Some(Row::Project { root, expanded, .. }) => set_expanded(ed, root, !expanded),
            Some(Row::Worktree { row, .. }) => {
                if row.missing {
                    ed.error(format!("{} is gone", row.path.display()));
                } else if super::switch::switch(ed, &row.path) {
                    ed.workspaces.sidebar.focused = false;
                }
            }
            Some(Row::Agent(a)) => match &a.worktree {
                Some(path) => {
                    if super::switch::switch(ed, path) {
                        ed.workspaces.sidebar.focused = false;
                    }
                }
                None => ed.info(format!("{} has no worktree yet", a.name)),
            },
            _ => {}
        },
        KeyCode::Char('l') | KeyCode::Right => {
            if let Some(Row::Project { root, .. }) = at.map(|i| &rows[i]) {
                set_expanded(ed, root, true);
            }
        }
        KeyCode::Char('h') | KeyCode::Left => match at.map(|i| &rows[i]) {
            Some(Row::Project { root, .. }) => set_expanded(ed, root, false),
            Some(Row::Worktree { project, .. }) => {
                let project = project.clone();
                set_expanded(ed, &project, false);
                ed.workspaces.sidebar.selected = Some(RowKey::Project(project));
            }
            _ => {}
        },
        _ => ed.info("sidebar: j/k move, ret open, h/l fold, r rescan, esc editor, q hide"),
    }
    true
}

fn set_expanded(ed: &mut Editor, root: &std::path::Path, on: bool) {
    let set = &mut ed.workspaces.sidebar.expanded;
    if on {
        set.insert(root.to_path_buf());
    } else {
        set.remove(root);
    }
}

/// The mark and word for a row's agent, for front ends: `! ready for
/// review`.
pub fn attention_text(a: Attention) -> String {
    format!("{} {}", a.symbol(), a.word())
}
