//! Navigation: the gitignore-aware file finder, workspace search, the picker
//! preview, session restore and recent files.
//!
//! Walking and searching run on background threads and stream results into
//! the open picker through [`stream::Stream`], so typing keeps working while
//! a large tree is listed (results never queue up behind each other; see
//! [`stream`]). Filtering a very large list still costs time per key.
//! Sessions and the recent-files list live in the state directory
//! (`$XDG_STATE_HOME/maestro`); with no state directory configured (tests,
//! `--clean`) nothing is read from or written to disk.

pub mod finder;
pub mod grep;
pub mod lines;
pub mod preview;
pub mod recent;
pub mod session;
mod stream;

use std::path::{Path, PathBuf};

use crate::editor::Editor;
use crate::picker::{Picker, PickerAction, PickerItem};

crate::commands! {
    file_picker => "Find a file in the working directory (respects .gitignore)",
    global_search => "Search every file in the workspace for a regex",
    recent_files => "Pick a recently opened file",
}

/// Default normal-mode bindings for the commands above that keymap.rs does
/// not already bind, as (keys, command). Bound under the space group.
pub const DEFAULT_KEYS: &[(&str, &str)] = &[("space r", "recent_files")];

/// Navigation state kept on the editor.
#[derive(Debug, Default)]
pub struct NavState {
    /// `$XDG_STATE_HOME/maestro`. `None` means sessions and recent files
    /// stay in memory only.
    pub state_dir: Option<PathBuf>,
    /// Save the session when the editor quits.
    pub save_session: bool,
    /// The directory the session belongs to: the working directory at
    /// startup, so `:cd` does not move the session elsewhere.
    pub session_cwd: Option<PathBuf>,
    /// Set by the first switch between worktrees (`crate::workspaces`):
    /// from then on a saved session holds only the files of its worktree.
    pub scoped: bool,
    /// Recently opened files, most recent first.
    pub recent: Vec<PathBuf>,
    /// Set while a session is restored, so reopening its files does not
    /// count as opening them.
    pub(crate) restoring: bool,
}

/// Sets where navigation state is stored and whether sessions are saved.
/// Called once at startup by the front end.
pub fn configure(ed: &mut Editor, state_dir: Option<PathBuf>, sessions: bool) {
    ed.navigation.recent = state_dir.as_deref().map(recent::load).unwrap_or_default();
    ed.navigation.state_dir = state_dir;
    ed.navigation.save_session = sessions;
    ed.navigation.session_cwd = Some(ed.cwd.clone());
}

fn file_picker(ed: &mut Editor) {
    finder::open(ed);
}

fn global_search(ed: &mut Editor) {
    crate::prompt::open(ed, crate::prompt::PromptKind::GlobalSearch);
}

fn recent_files(ed: &mut Editor) {
    recent::open_picker(ed);
}

/// Path shown in pickers: relative to `root` when inside it, otherwise with
/// the home directory abbreviated to `~`. Control characters in file names
/// are shown as `?` so they cannot reach the terminal.
pub fn display_path(path: &Path, root: &Path) -> String {
    let shown = if let Ok(rel) = path.strip_prefix(root) {
        rel.to_string_lossy().into_owned()
    } else if let Some(home) = std::env::var_os("HOME")
        && let Ok(rel) = path.strip_prefix(&home)
    {
        format!("~/{}", rel.to_string_lossy())
    } else {
        path.to_string_lossy().into_owned()
    };
    if shown.chars().any(char::is_control) {
        return shown
            .chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .collect();
    }
    shown
}

/// Opens a picker that background work will fill, and returns its id.
fn open_streaming_picker(ed: &mut Editor, title: String) -> u64 {
    let mut picker = Picker::new(title, Vec::new());
    picker.loading = true;
    let id = picker.id;
    ed.picker = Some(picker);
    id
}

fn file_item(label: String, path: PathBuf) -> PickerItem {
    PickerItem {
        label,
        detail: String::new(),
        action: PickerAction::OpenFile(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_path_is_relative_inside_root() {
        let root = Path::new("/work/proj");
        assert_eq!(
            display_path(Path::new("/work/proj/src/a.rs"), root),
            "src/a.rs"
        );
        assert_eq!(
            display_path(Path::new("/elsewhere/b.rs"), root),
            "/elsewhere/b.rs"
        );
        assert_eq!(
            display_path(Path::new("/work/proj/a\x1b[2Jb\n.rs"), root),
            "a?[2Jb?.rs"
        );
    }
}
