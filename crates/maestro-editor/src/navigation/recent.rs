//! Recently opened files, across sessions: `<state>/recent.toml`, most
//! recent first, capped at [`LIMIT`]. Every open rereads the file before
//! writing it, so editors running at once see each other's opens. The
//! read and write are not locked: two opens in different editors at the
//! same instant can lose one entry.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::editor::Editor;
use crate::picker::Picker;

pub const LIMIT: usize = 200;
const FILE: &str = "recent.toml";

#[derive(Debug, Default, Serialize, Deserialize)]
struct RecentFile {
    version: u32,
    files: Vec<PathBuf>,
}

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE)
}

/// Reads the list. A missing or unreadable file is an empty list.
pub fn load(state_dir: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(path(state_dir))
        .ok()
        .and_then(|s| toml::from_str::<RecentFile>(&s).ok())
        .map(|r| r.files)
        .unwrap_or_default()
}

/// Puts `file` first, dropping an older entry for it and anything past
/// [`LIMIT`].
pub fn push_front(list: &mut Vec<PathBuf>, file: PathBuf) {
    list.retain(|p| *p != file);
    list.insert(0, file);
    list.truncate(LIMIT);
}

/// Records that `file` was opened. Writes the list when a state directory
/// is configured; failures to write are ignored (the list is a convenience).
pub fn record(ed: &mut Editor, file: &Path) {
    if ed.navigation.restoring {
        return;
    }
    let file = crate::editor::normalize_path(file);
    if file.to_str().is_none() {
        return;
    }
    // The file on disk includes other editors' opens; fall back to memory
    // when it is missing or unreadable.
    let mut list = ed
        .navigation
        .state_dir
        .as_deref()
        .map(load)
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| std::mem::take(&mut ed.navigation.recent));
    push_front(&mut list, file);
    if let Some(dir) = &ed.navigation.state_dir {
        let body = RecentFile {
            version: 1,
            files: list.clone(),
        };
        if let Ok(text) = toml::to_string(&body) {
            let _ = super::session::write_private(&path(dir), text.as_bytes(), false);
        }
    }
    ed.navigation.recent = list;
}

/// Opens the recent-files picker. Files that no longer exist are left out.
pub fn open_picker(ed: &mut Editor) {
    if let Some(dir) = &ed.navigation.state_dir {
        ed.navigation.recent = load(dir);
    }
    let root = ed.cwd.clone();
    let items: Vec<_> = ed
        .navigation
        .recent
        .iter()
        .filter(|p| p.is_file())
        .map(|p| super::file_item(super::display_path(p, &root), p.clone()))
        .collect();
    ed.picker = Some(Picker::new("recent files", items));
    super::preview::refresh(ed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_front_dedupes_and_caps() {
        let mut list: Vec<PathBuf> = (0..LIMIT)
            .map(|i| PathBuf::from(format!("/f{i}")))
            .collect();
        push_front(&mut list, "/f5".into());
        assert_eq!(list[0], PathBuf::from("/f5"));
        assert_eq!(list.len(), LIMIT);
        assert_eq!(list.iter().filter(|p| **p == Path::new("/f5")).count(), 1);
        push_front(&mut list, "/new".into());
        assert_eq!(list.len(), LIMIT);
        assert_eq!(list[0], PathBuf::from("/new"));
        assert_eq!(list[1], PathBuf::from("/f5"));
    }
}
