//! The file finder: walks the working directory on a background thread,
//! honoring `.gitignore`, `.ignore`, git's global excludes and
//! `.git/info/exclude`, skipping hidden files, and streams paths into the
//! picker as it goes.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use super::stream::Stream;
use crate::editor::{Editor, JobSender};

/// The finder stops listing after this many files and says so in the title.
pub const FILE_LIMIT: usize = 1_000_000;

/// A walker with Maestro's ignore rules. `.gitignore` applies even outside
/// a Git repository (like `rg --no-require-git`), so a project that is not
/// yet a repository still hides its build output.
pub fn walker(root: &Path) -> WalkBuilder {
    let mut b = WalkBuilder::new(root);
    b.hidden(true)
        .require_git(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .follow_links(false)
        .filter_entry(|e| e.file_name() != ".git");
    b
}

/// Opens the file picker and starts the background walk.
pub fn open(ed: &mut Editor) {
    let root = ed.cwd.clone();
    let id = super::open_streaming_picker(ed, "files".into());
    if let Err(e) = ed.spawn_background("file-finder", move |tx| walk(root, tx, id)) {
        if let Some(p) = &mut ed.picker {
            p.loading = false;
        }
        ed.error(format!("could not start the file finder: {e}"));
    }
}

fn walk(root: PathBuf, tx: JobSender, picker_id: u64) {
    let mut stream = Stream::new(tx, picker_id);
    let mut walk = walker(&root);
    walk.sort_by_file_name(|a, b| a.cmp(b));
    for entry in walk.build() {
        if stream.cancelled() {
            return;
        }
        stream.tick();
        let Ok(entry) = entry else { continue };
        let Some(ft) = entry.file_type() else {
            continue;
        };
        // A symlink is listed when it points at a file.
        if !(ft.is_file() || (ft.is_symlink() && entry.path().is_file())) {
            continue;
        }
        let path = entry.into_path();
        let label = super::display_path(&path, &root);
        stream.push(super::file_item(label, path));
        if stream.count() >= FILE_LIMIT {
            stream.finish(Some(format!("files (first {FILE_LIMIT})")));
            return;
        }
    }
    stream.finish(None);
}
