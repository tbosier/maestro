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

/// Path shown in pickers and messages: relative to `root` when inside it,
/// otherwise as [`home_path`] shows it. `root` itself is shown by
/// [`home_path`] too, since its relative form would be empty. A relative
/// path whose first part is a directory named `~` gets a leading `./`, so
/// it cannot be read as the home directory. Control characters in file
/// names are shown as `?` so they cannot reach the terminal.
pub fn display_path(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => {
            let shown = rel.to_string_lossy().into_owned();
            if rel.components().next() == Some(std::path::Component::Normal("~".as_ref())) {
                printable(format!("./{shown}"))
            } else {
                printable(shown)
            }
        }
        _ => home_path(path),
    }
}

/// `path` with the home directory abbreviated to `~` (`~/src/app`, or `~`
/// for home itself), absolute otherwise. For a directory the working
/// directory just changed to, where a relative form says nothing, and for
/// text that outlives the working directory (agent history) or is built
/// where it is not known.
pub fn home_path(path: &Path) -> String {
    abbreviate_home(path, home_dirs())
}

/// HOME as set, and with symlinks resolved when that differs: `:cd`
/// canonicalizes its target, so a home reached through a symlink must
/// match too. Read once; nothing in the editor changes HOME.
fn home_dirs() -> &'static [PathBuf] {
    static HOMES: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
    HOMES.get_or_init(|| {
        let Some(home) = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .filter(|h| h.is_absolute())
        else {
            return Vec::new();
        };
        let mut homes = vec![home.clone()];
        if let Ok(real) = std::fs::canonicalize(&home)
            && real != home
        {
            homes.push(real);
        }
        homes
    })
}

/// [`home_path`] with the home directories given. A home of `/` is not
/// abbreviated: every path would start with `~`.
fn abbreviate_home(path: &Path, homes: &[PathBuf]) -> String {
    let rel = homes
        .iter()
        .filter(|h| h.parent().is_some())
        .find_map(|h| path.strip_prefix(h).ok());
    let shown = match rel {
        Some(rel) if rel.as_os_str().is_empty() => "~".to_string(),
        Some(rel) => format!("~/{}", rel.to_string_lossy()),
        None => path.to_string_lossy().into_owned(),
    };
    printable(shown)
}

/// `s` with control characters replaced by `?`.
fn printable(s: String) -> String {
    if s.chars().any(char::is_control) {
        return s
            .chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .collect();
    }
    s
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
        // The root itself has no relative form.
        assert_eq!(display_path(root, root), home_path(root));
        // A directory named `~` is not the home directory.
        assert_eq!(display_path(Path::new("/work/proj/~/x"), root), "./~/x");
    }

    #[test]
    fn home_is_abbreviated_through_a_symlink_but_not_when_it_is_root() {
        let homes = [PathBuf::from("/link/me"), PathBuf::from("/real/me")];
        assert_eq!(abbreviate_home(Path::new("/link/me"), &homes), "~");
        assert_eq!(
            abbreviate_home(Path::new("/real/me/src/app"), &homes),
            "~/src/app"
        );
        assert_eq!(
            abbreviate_home(Path::new("/real/me/a\x07b"), &homes),
            "~/a?b"
        );
        assert_eq!(
            abbreviate_home(Path::new("/elsewhere/x"), &homes),
            "/elsewhere/x"
        );
        assert_eq!(
            abbreviate_home(Path::new("/elsewhere/x"), &[]),
            "/elsewhere/x"
        );
        let root_home = [PathBuf::from("/")];
        assert_eq!(abbreviate_home(Path::new("/etc/x"), &root_home), "/etc/x");
    }
}
