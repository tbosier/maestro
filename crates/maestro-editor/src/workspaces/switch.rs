//! Switching the editor to another worktree.
//!
//! What moves: the working directory (and with it the file finder,
//! workspace search, the agent panel's repository and relative paths),
//! the directory the session is saved for, and, through the buffers that
//! open, Git signs and language servers (servers start on demand for the
//! new root; see `lsp::stop_unused` for when the old ones stop).
//!
//! The policy for open buffers:
//!
//! * The files of the worktree you leave are saved as its session (with
//!   the split layout and cursors), and those without unsaved changes are
//!   closed.
//! * A buffer with unsaved changes is never closed: it stays open, keeps
//!   its edits and undo history, and is named in the status line. It is in
//!   the buffer list (`space b`) and the jump palette, and the sidebar
//!   counts it on its worktree's row. Quitting still refuses while it is
//!   unsaved.
//! * Files outside the worktree you leave (another worktree's kept
//!   buffers, files outside every worktree) stay open.
//! * The target's saved session is restored: its files open and its
//!   layout comes back. Without one the editor shows an empty buffer.
//!
//! Sessions are saved to the state directory as on quit; with sessions
//! off (`--clean`) they are kept in memory for this run only.

use std::path::{Path, PathBuf};

use maestro_core::Document;

use crate::editor::{DocumentId, Editor, Severity, ViewId, normalize_path};
use crate::layout::Layout;
use crate::navigation::session;

/// Switches the editor to the worktree at `target`. Returns false (with a
/// message) when the target is not a directory.
pub fn switch(ed: &mut Editor, target: &Path) -> bool {
    let target = match std::fs::canonicalize(target) {
        Ok(p) if p.is_dir() => p,
        Ok(p) => {
            ed.error(format!("{} is not a directory", p.display()));
            return false;
        }
        Err(e) => {
            ed.error(format!("cannot switch to {}: {e}", target.display()));
            return false;
        }
    };
    let old = super::current_root(ed);
    if normalize_path(&old) == target {
        let name = label(ed, &target);
        ed.info(format!("already in {name}"));
        return true;
    }
    if let Err(e) = save_session(ed, &old) {
        // The buffers stay open below only when unsaved; a session that
        // could not be written must not close the others silently.
        ed.error(format!("not switching: {e}"));
        return false;
    }

    let mut closing: Vec<DocumentId> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for b in ed.buffers.values() {
        let Some(path) = b.doc.path() else {
            continue;
        };
        if !super::owned_by(ed, path, &old) {
            continue;
        }
        if b.doc.is_modified() {
            kept.push(crate::navigation::display_path(path, &old));
        } else {
            closing.push(b.id);
        }
    }

    reset_views(ed);
    for id in closing {
        // Unmodified, so this never fails; a buffer that changed in the
        // meantime would be refused, not lost.
        let _ = ed.close_buffer(id, false);
    }
    ed.cwd = target.clone();
    ed.navigation.session_cwd = Some(target.clone());
    ed.navigation.scoped = true;
    let restored = restore_session(ed, &target);
    crate::lsp::reconcile(ed);
    super::scan::request(ed);

    let name = label(ed, &target);
    let mut msg = format!("switched to {name}");
    let mut severity = Severity::Info;
    match restored {
        Some((sev, text)) => {
            severity = severity.max(sev);
            msg.push_str(&format!("; {text}"));
        }
        None => msg.push_str("; no saved session"),
    }
    if !kept.is_empty() {
        severity = severity.max(Severity::Warning);
        msg.push_str(&format!(
            "; kept open with unsaved changes: {} (from {})",
            kept.join(", "),
            label(ed, &old)
        ));
    }
    ed.set_status(severity, msg);
    true
}

/// `project:worktree` when the scan knows the path, else the path.
fn label(ed: &Editor, root: &Path) -> String {
    match super::names_for(ed, root) {
        Some((p, w)) => format!("{p}:{w}"),
        None => root.display().to_string(),
    }
}

/// Saves the files of the worktree at `root` as its session: to the state
/// directory when sessions are saved, else in memory.
fn save_session(ed: &mut Editor, root: &Path) -> Result<(), String> {
    let root_buf: PathBuf = root.to_path_buf();
    let captured = session::capture_where(ed, root, &|p| super::owned_by(ed, p, &root_buf));
    let nav = &ed.navigation;
    match (&nav.state_dir, nav.save_session) {
        (Some(dir), true) => session::write(dir, root, &captured),
        _ => {
            let key = normalize_path(root);
            if captured.buffers.is_empty() {
                ed.workspaces.memory_sessions.remove(&key);
            } else {
                ed.workspaces.memory_sessions.insert(key, captured);
            }
            Ok(())
        }
    }
}

/// Opens the saved session of `root`, if there is one, and returns what
/// to tell the user.
fn restore_session(ed: &mut Editor, root: &Path) -> Option<(Severity, String)> {
    let nav = &ed.navigation;
    let saved = match (&nav.state_dir, nav.save_session) {
        (Some(dir), true) => match session::load(dir, root) {
            Ok(s) => s,
            Err(e) => return Some((Severity::Warning, e)),
        },
        _ => ed.workspaces.memory_sessions.get(root).cloned(),
    };
    let saved = saved.filter(|s| !s.buffers.is_empty())?;
    Some(session::apply(ed, &saved, root))
}

/// Replaces every view with one view of a new empty buffer. Each buffer
/// remembers the selection its view had.
fn reset_views(ed: &mut Editor) {
    let scratch = ed.add_document(Document::default());
    let view = ed.new_view(scratch);
    let old: Vec<ViewId> = ed.views.keys().copied().filter(|v| *v != view).collect();
    for v in old {
        if let Some(gone) = ed.views.remove(&v)
            && let Some(b) = ed.buffers.get_mut(&gone.doc)
        {
            b.last_selection = gone.selection;
        }
    }
    ed.layout = Layout::new(view);
    ed.focus = view;
    ed.relayout();
    let unnamed: Vec<DocumentId> = ed
        .buffers
        .values()
        .filter(|b| b.doc.path().is_none() && b.id != scratch)
        .map(|b| b.id)
        .collect();
    for id in unnamed {
        ed.remove_scratch_if_unused(id);
    }
}
