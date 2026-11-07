//! The jump palette (`space j`): one fuzzy list of projects, worktrees,
//! agents (by name and task) and open buffers. Choosing one goes there,
//! switching worktree when needed.
//!
//! Projects and worktrees come from the last scan. Before the first scan
//! finishes the palette shows agents and buffers, says it is loading, and
//! the rest is added when the scan arrives (if the palette is still open).
//! The last entry, "add a project", asks for a path to add
//! ([`super::manage`]).

use std::path::PathBuf;

use crate::editor::{DocumentId, Editor};
use crate::picker::{Picker, PickerAction, PickerItem};

/// Where a palette entry goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Jump {
    /// Switch to this worktree.
    Worktree(PathBuf),
    /// Show this buffer, after switching to the worktree it belongs to.
    Buffer(DocumentId),
}

pub const TITLE: &str = "jump";

pub fn open(ed: &mut Editor) {
    super::scan::request(ed);
    let mut picker = Picker::new(TITLE, items(ed));
    picker.loading = !ed.workspaces.scanned;
    ed.picker = Some(picker);
}

/// Called when a scan finishes: a palette still waiting for the first
/// scan gets the projects and worktrees.
pub(crate) fn scan_finished(ed: &mut Editor) {
    let waiting = ed
        .picker
        .as_ref()
        .is_some_and(|p| p.loading && p.title == TITLE);
    if !waiting {
        return;
    }
    let all = items(ed);
    if let Some(p) = &mut ed.picker {
        let new: Vec<PickerItem> = all.into_iter().filter(|i| !p.items().contains(i)).collect();
        p.loading = false;
        p.extend(new);
    }
}

/// Every entry, in the order shown before typing: agents by attention,
/// then projects with their worktrees, then buffers, then "add a project"
/// (once a scan has listed the projects, so it stays last).
pub fn items(ed: &Editor) -> Vec<PickerItem> {
    let mut out = Vec::new();
    for a in super::agent_entries(ed) {
        let Some(path) = a.worktree.clone() else {
            continue;
        };
        let task = a.task.lines().next().unwrap_or_default().trim().to_string();
        out.push(PickerItem {
            label: if task.is_empty() {
                format!("agent {}", a.name)
            } else {
                format!("agent {}: {task}", a.name)
            },
            detail: a.state_text(),
            action: PickerAction::Jump(Jump::Worktree(path)),
        });
    }
    for p in &ed.workspaces.projects {
        // A bare repository lists no main worktree: the project then goes
        // to its first linked one.
        let first = p
            .worktrees
            .iter()
            .filter(|w| !w.missing)
            .min_by_key(|w| !w.is_main);
        if let Some(main) = first {
            out.push(PickerItem {
                label: format!("project {}", p.name),
                // A short detail: a long path would cover the label.
                detail: format!(
                    "{} worktree{}",
                    p.worktrees.len(),
                    if p.worktrees.len() == 1 { "" } else { "s" }
                ),
                action: PickerAction::Jump(Jump::Worktree(main.path.clone())),
            });
        }
        for w in p.worktrees.iter().filter(|w| !w.missing) {
            let name = if w.is_main { "main" } else { w.name.as_str() };
            let mut detail = Vec::new();
            if let Some(n) = w.dirty {
                detail.push(format!("{n} changed"));
            }
            if let Some(u) = &w.upstream {
                detail.push(format!("ahead {} behind {}", u.ahead, u.behind));
            }
            out.push(PickerItem {
                label: format!("worktree {}:{name} {}", p.name, w.branch_label()),
                detail: detail.join(", "),
                action: PickerAction::Jump(Jump::Worktree(w.path.clone())),
            });
        }
    }
    for b in ed.buffers.values() {
        let Some(path) = b.doc.path() else {
            // Unnamed buffers with text (a kept scratch, a log) are listed
            // too; an empty one is not worth a row.
            if b.doc.text().len_chars() > 0 {
                let first = b.doc.text().line(0).to_string();
                out.push(PickerItem {
                    label: format!(
                        "buffer [scratch] {}",
                        first.trim().chars().take(60).collect::<String>()
                    ),
                    detail: if b.doc.is_modified() {
                        "unsaved".into()
                    } else {
                        String::new()
                    },
                    action: PickerAction::Jump(Jump::Buffer(b.id)),
                });
            }
            continue;
        };
        let owner = super::owner_of(ed, path);
        let shown = match &owner {
            Some(root) => crate::navigation::display_path(path, root),
            None => crate::navigation::display_path(path, &ed.cwd),
        };
        let mut detail = owner
            .as_deref()
            .and_then(|r| super::names_for(ed, r))
            .map(|(p, w)| format!("{p}:{w}"))
            .unwrap_or_default();
        if b.doc.is_modified() {
            detail = format!("unsaved {detail}").trim().to_string();
        }
        out.push(PickerItem {
            label: format!("buffer {shown}"),
            detail,
            action: PickerAction::Jump(Jump::Buffer(b.id)),
        });
    }
    if ed.workspaces.scanned {
        out.push(PickerItem {
            label: "add a project".into(),
            detail: "a path in a Git repository".into(),
            action: PickerAction::Command("project-add".into()),
        });
    }
    out
}

/// Goes where a palette entry points.
pub fn go(ed: &mut Editor, jump: Jump) {
    match jump {
        Jump::Worktree(path) => {
            if super::switch::switch(ed, &path) {
                ed.workspaces.sidebar.focused = false;
            }
        }
        Jump::Buffer(id) => {
            let Some(path) = ed
                .buffers
                .get(&id)
                .map(|b| b.doc.path().map(|p| p.to_path_buf()))
            else {
                ed.error("that buffer was closed");
                return;
            };
            if let Some(owner) = path.as_deref().and_then(|p| super::owner_of(ed, p))
                && owner != super::current_root(ed)
                && !super::switch::switch(ed, &owner)
            {
                return;
            }
            if ed.buffers.contains_key(&id) {
                ed.switch_to(id);
            }
        }
    }
}
