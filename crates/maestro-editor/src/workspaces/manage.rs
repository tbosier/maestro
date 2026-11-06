//! Adding and forgetting projects from inside the editor.
//!
//! * `a` in the sidebar, `:project-add` and the jump palette's "add a
//!   project" entry ask for a path (`~` and paths relative to the working
//!   directory work; `tab` completes directory names). A background job
//!   checks it is a directory in a Git repository and resolves it to the
//!   repository's main worktree, the way the scan keys projects. The
//!   project then shows in the sidebar, selected and unfolded.
//! * `d` on a project row, or `:project-remove <name or path>`, forgets a
//!   project after a y/n question. It only leaves the list: no file,
//!   worktree or branch is touched. Projects from `[projects] paths` are
//!   refused (the config file is hand-written; remove them there), and so
//!   are the project the editor is in and projects an agent of this editor
//!   works in, since those are always listed.
//!
//! The list is kept in `<state>/projects.toml`, never in `config.toml`:
//!
//! ```toml
//! version = 1
//! added = ["/home/me/src/site"]
//! hidden = ["/home/me/src/old"]
//! ```
//!
//! `added` holds the projects you added. `hidden` holds the projects you
//! forgot: a forgotten project stays off the list even when a saved
//! session or an earlier scan names it, until you add it again. Both hold
//! main worktree paths. With no state directory (`--clean`, headless, the
//! tests) the file is neither read nor written and changes last for the
//! run. The file is untrusted input: a file that cannot be used is ignored
//! with a note in the status line, and the next change replaces it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::scan::{self, Project, Unusable};
use super::sidebar::RowKey;
use crate::editor::{Editor, Severity};
use crate::prompt::{self, PromptKind};

/// The file in the state directory.
pub const FILE: &str = "projects.toml";

const VERSION: u32 = 1;
/// Larger files are not read, and a list that would be larger is not
/// written.
const MAX_BYTES: u64 = 1 << 20;
/// Entries past this many per list are dropped when read. Adding past it
/// is refused; forgetting past it drops the oldest hidden entry.
const MAX_ENTRIES: usize = 1000;

/// The projects added and forgotten in the editor.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProjectList {
    /// Main worktrees of the projects added. For a repository whose main
    /// worktree is bare, the linked worktree that was added instead (a
    /// bare directory is not a project the scan can list).
    pub added: Vec<PathBuf>,
    /// Main worktrees of the projects forgotten, oldest first.
    pub hidden: Vec<PathBuf>,
}

impl ProjectList {
    fn add(&mut self, entry: &Path, root: &Path) {
        self.added.retain(|p| p != entry);
        self.added.push(entry.to_path_buf());
        self.hidden.retain(|p| p != root);
    }

    /// Drops every added entry `belongs` says is this project's and hides
    /// `root`.
    fn forget(&mut self, root: &Path, belongs: &dyn Fn(&Path) -> bool) {
        self.added.retain(|p| !belongs(p));
        if !self.hidden.iter().any(|p| p == root) {
            self.hidden.push(root.to_path_buf());
        }
        if self.hidden.len() > MAX_ENTRIES {
            let extra = self.hidden.len() - MAX_ENTRIES;
            self.hidden.drain(..extra);
        }
    }
}

/// Whether a saved entry names the project at `root` with these worktree
/// paths: the root itself, a path that resolves to it (a symlink left
/// where the repository was), or a path inside one of its worktrees.
fn belongs_to(root: &Path, worktrees: &[PathBuf]) -> impl Fn(&Path) -> bool + use<> {
    let root = root.to_path_buf();
    let worktrees = worktrees.to_vec();
    move |p: &Path| {
        p == root
            || worktrees.iter().any(|w| p.starts_with(w))
            || crate::editor::normalize_path(p) == root
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ListFile {
    version: u32,
    #[serde(default)]
    added: Vec<String>,
    #[serde(default)]
    hidden: Vec<String>,
}

/// `<state>/projects.toml`.
pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE)
}

/// Reads the list. `Ok(None)` when there is no file; `Err` says why a
/// file cannot be used. Relative and repeated entries are dropped.
pub fn read(file: &Path) -> Result<Option<ProjectList>, String> {
    use std::io::Read;
    let meta = match std::fs::symlink_metadata(file) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    // Only small regular files: a FIFO or a huge file must not block. The
    // opened file is checked again and read no further than the limit.
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let f = std::fs::File::open(file).map_err(|e| e.to_string())?;
    let meta = f.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let mut bytes = Vec::new();
    f.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("larger than 1 MiB".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "not valid UTF-8".to_string())?;
    let parsed: ListFile = toml::from_str(&text).map_err(|e| {
        let msg = e.message().trim().to_string();
        if msg.is_empty() {
            "not valid TOML".to_string()
        } else {
            msg
        }
    })?;
    if parsed.version != VERSION {
        return Err(format!("version {} is not {VERSION}", parsed.version));
    }
    let paths = |mut list: Vec<String>| -> Vec<PathBuf> {
        list.truncate(MAX_ENTRIES);
        let mut seen = std::collections::HashSet::new();
        list.into_iter()
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && seen.insert(p.clone()))
            .collect()
    };
    Ok(Some(ProjectList {
        added: paths(parsed.added),
        hidden: paths(parsed.hidden),
    }))
}

/// Reads `<state>/projects.toml` once, on the first scan. A file that
/// cannot be used is ignored and the status line says so.
pub(crate) fn load(ed: &mut Editor) {
    if ed.workspaces.saved_loaded {
        return;
    }
    ed.workspaces.saved_loaded = true;
    let Some(state) = ed.navigation.state_dir.clone() else {
        return;
    };
    let file = path(&state);
    match read(&file) {
        Ok(Some(list)) => ed.workspaces.saved = list,
        Ok(None) => {}
        Err(why) => ed.set_status(
            Severity::Warning,
            format!(
                "ignored {}: {why}; adding or forgetting a project replaces it",
                file.display()
            ),
        ),
    }
}

/// What happened to a change on disk.
enum Saved {
    Yes,
    /// No state directory: the change lasts for this run.
    NoStateDir,
    Failed(String),
}

/// Applies `change` to the list and writes it. The file is read again
/// first, so projects another editor added since are kept; the read and
/// the write are not locked, so two editors saving at the same instant
/// can lose one change (as with recent files). After a failed write the
/// list in memory is the base until a write succeeds, so changes that
/// only live in this run are not dropped. `root` is the project that
/// changed: a root that is not UTF-8 cannot be written and lasts for the
/// run.
fn save(ed: &mut Editor, root: &Path, change: impl Fn(&mut ProjectList)) -> Saved {
    load(ed);
    let Some(state) = ed.navigation.state_dir.clone() else {
        change(&mut ed.workspaces.saved);
        return Saved::NoStateDir;
    };
    let file = path(&state);
    let mut list = match read(&file) {
        Ok(Some(l)) if !ed.workspaces.saved_unwritten => l,
        _ => ed.workspaces.saved.clone(),
    };
    change(&mut list);
    ed.workspaces.saved = list.clone();
    // Paths that are not UTF-8 stay in memory only.
    let strings = |v: &[PathBuf]| -> Vec<String> {
        v.iter()
            .filter_map(|p| p.to_str().map(str::to_string))
            .collect()
    };
    let body = ListFile {
        version: VERSION,
        added: strings(&list.added),
        hidden: strings(&list.hidden),
    };
    let written = match toml::to_string(&body) {
        Ok(t) if t.len() as u64 > MAX_BYTES => Err("the list would be larger than 1 MiB".into()),
        Ok(t) => crate::navigation::session::write_private(&file, t.as_bytes(), true)
            .map_err(|e| format!("could not write {}: {e}", file.display())),
        Err(e) => Err(e.to_string()),
    };
    ed.workspaces.saved_unwritten = written.is_err();
    match written {
        Ok(()) if root.to_str().is_none() => Saved::Failed("the path is not valid UTF-8".into()),
        Ok(()) => Saved::Yes,
        Err(why) => Saved::Failed(why),
    }
}

/// A note for the status line when a change was not written.
fn saved_note(saved: &Saved) -> String {
    match saved {
        Saved::Yes => String::new(),
        Saved::NoStateDir => " (for this run only: no state directory)".into(),
        Saved::Failed(why) => format!(" (for this run only: {why})"),
    }
}

// ----- adding --------------------------------------------------------------

/// Opens the "add project" prompt, filled in with the parent of the
/// working directory.
pub fn open_prompt(ed: &mut Editor) {
    let start = prefill(&ed.cwd);
    prompt::open(ed, PromptKind::ProjectAdd);
    if let Some(p) = ed.prompt.as_mut() {
        p.input = start;
        p.cursor = p.input.len();
    }
}

/// The parent of `cwd` with a trailing `/`, the home directory written
/// as `~`.
fn prefill(cwd: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let Some(parent) = cwd.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return "~/".into();
    };
    let shown = match home.as_deref().and_then(|h| parent.strip_prefix(h).ok()) {
        Some(rel) if rel.as_os_str().is_empty() => "~".to_string(),
        Some(rel) => format!("~/{}", rel.display()),
        None => parent.display().to_string(),
    };
    if shown.chars().any(char::is_control) || parent.to_str().is_none() {
        return "~/".into();
    }
    if shown.ends_with('/') {
        shown
    } else {
        format!("{shown}/")
    }
}

/// The "add project" prompt was accepted with `input`.
pub(crate) fn prompt_done(ed: &mut Editor, input: &str) {
    add(ed, input);
}

/// Checks `input` (a path, `~` and relative paths allowed) in the
/// background and adds the project when it is a directory in a Git
/// repository.
pub fn add(ed: &mut Editor, input: &str) {
    let input = input.trim();
    if input.is_empty() {
        ed.info("add project canceled: no path given");
        return;
    }
    // Not shortened lexically: `link/..` must mean what the filesystem
    // says it means.
    let dir = ed.resolve_path(Path::new(input));
    let git = ed.git.git().clone();
    let state = ed.navigation.state_dir.clone();
    let records = crate::agents::persist::enabled(ed).is_some();
    let generation = ed.workspaces.scan.generation;
    let checked = dir.clone();
    let spawned = ed.spawn_background("project-add", move |tx| {
        let found = scan::locate(&git, &checked).map(|(main, list)| {
            let project = scan::build(&git, main, &list, state.as_deref(), records);
            (project, crate::editor::normalize_path(&checked))
        });
        tx.send(move |ed| finish_add(ed, &checked, generation, found));
    });
    match spawned {
        Ok(()) => ed.info(format!("checking {}…", dir.display())),
        Err(e) => ed.error(format!("could not check {}: {e}", dir.display())),
    }
}

fn finish_add(
    ed: &mut Editor,
    dir: &Path,
    generation: u64,
    found: Result<(Project, PathBuf), Unusable>,
) {
    let (project, canonical) = match found {
        Ok(p) => p,
        Err(why) => {
            let shown = dir.display();
            ed.error(match why {
                Unusable::Missing => format!("no such directory: {shown}"),
                Unusable::NotDirectory => format!("not a directory: {shown}"),
                Unusable::NotRepository => format!("not a Git repository: {shown}"),
                Unusable::Git(e) => format!("could not read the repository at {shown}: {e}"),
            });
            return;
        }
    };
    let root = project.root.clone();
    let name = project.name.clone();
    if ed.workspaces.scan.generation != generation && ed.workspaces.saved.hidden.contains(&root) {
        ed.info(format!(
            "{name} was forgotten while it was being checked, so it was not added"
        ));
        return;
    }
    if root.to_str().is_none() {
        ed.error(format!(
            "cannot keep a project whose path is not valid UTF-8: {}",
            root.display()
        ));
        return;
    }
    let worktrees: Vec<PathBuf> = project.worktrees.iter().map(|w| w.path.clone()).collect();
    let belongs = belongs_to(&root, &worktrees);
    // A bare main worktree is not a directory the scan can list: keep the
    // linked worktree that was added (the deepest one holding the path).
    let entry = if project.worktrees.iter().any(|w| w.is_main) {
        root.clone()
    } else {
        worktrees
            .iter()
            .filter(|w| canonical.starts_with(w))
            .max_by_key(|w| w.as_os_str().len())
            .or(worktrees.first())
            .cloned()
            .unwrap_or_else(|| root.clone())
    };
    let configured = ed.workspaces.configured.contains(&root);
    let was_added = ed.workspaces.saved.added.iter().any(|p| belongs(p));
    if !configured && !was_added && ed.workspaces.saved.added.len() >= MAX_ENTRIES {
        ed.error(format!(
            "the list already holds {MAX_ENTRIES} added projects; forget one first"
        ));
        return;
    }
    let saved = if configured {
        None
    } else {
        Some(save(ed, &root, |l| l.add(&entry, &root)))
    };
    let ws = &mut ed.workspaces;
    ws.scan.generation += 1;
    match ws.projects.iter_mut().find(|p| p.root == root) {
        Some(p) => *p = project,
        None => ws.projects.push(project),
    }
    ws.sidebar.expanded.insert(root.clone());
    ws.sidebar.selected = Some(RowKey::Project(root.clone()));
    // Before the first scan the sidebar shows only that it is loading;
    // the selection is set again when the scan arrives.
    ws.select_after_scan = Some(root.clone());
    let mut msg = match &saved {
        None => format!("{name} is already listed: it is in [projects] paths"),
        Some(_) if was_added => format!("{name} is already in the list"),
        Some(s) => format!("added {name}{}", saved_note(s)),
    };
    if !ed.workspaces.sidebar.open {
        msg.push_str("; space s shows it");
    }
    match saved {
        Some(Saved::Failed(_)) => ed.set_status(Severity::Warning, msg),
        _ => ed.info(msg),
    }
    scan::request(ed);
}

// ----- forgetting ----------------------------------------------------------

/// Asks whether to forget the project at `root`, or says why it cannot be
/// forgotten.
pub fn forget(ed: &mut Editor, root: &Path) {
    let Some(p) = ed.workspaces.projects.iter().find(|p| p.root == root) else {
        ed.error(format!("{} is not in the list", root.display()));
        return;
    };
    let name = p.name.clone();
    let current = super::current_root(ed);
    let here = p.worktrees.iter().any(|w| w.path == current);
    let agent = ed
        .agents
        .agents
        .iter()
        .any(|a| a.worktree.as_ref().is_some_and(|w| w.main_root == root));
    if ed.workspaces.configured.iter().any(|c| c == root) {
        let file = match &ed.workspaces.config_path {
            Some(f) => f.display().to_string(),
            None => "the config file".into(),
        };
        ed.error(format!(
            "{name} is listed in [projects] paths in {file}; remove it there"
        ));
        return;
    }
    if here {
        ed.error(format!(
            "{name} is the project you are in, which is always listed; switch to another \
             project first"
        ));
        return;
    }
    if agent {
        ed.error(format!(
            "an agent of this editor works in {name}, so it stays listed while the agent is loaded"
        ));
        return;
    }
    // The question goes in the status line; the short label on its right
    // must not cover it.
    ed.info(format!(
        "forget {name}? It only leaves this list; no files are deleted"
    ));
    let root = root.to_path_buf();
    ed.on_next_char("y/n", move |ed, c| {
        if c == 'y' {
            finish_forget(ed, &root, &name);
        } else {
            ed.info(format!("kept {name}"));
        }
    });
}

fn finish_forget(ed: &mut Editor, root: &Path, name: &str) {
    let worktrees: Vec<PathBuf> = ed
        .workspaces
        .projects
        .iter()
        .filter(|p| p.root == root)
        .flat_map(|p| p.worktrees.iter().map(|w| w.path.clone()))
        .collect();
    let belongs = belongs_to(root, &worktrees);
    let saved = save(ed, root, |l| l.forget(root, &belongs));
    let ws = &mut ed.workspaces;
    ws.scan.generation += 1;
    let at = ws.projects.iter().position(|p| p.root == root);
    ws.projects.retain(|p| p.root != root);
    ws.sidebar.expanded.remove(root);
    if ws.sidebar.selected == Some(RowKey::Project(root.to_path_buf())) {
        // The project that took its place, else the one above.
        let next = at.and_then(|i| {
            ws.projects
                .get(i)
                .or_else(|| ws.projects.get(i.checked_sub(1)?))
        });
        ws.sidebar.selected = next.map(|p| RowKey::Project(p.root.clone()));
    }
    super::sidebar::pin_selection(ed);
    let msg = format!(
        "forgot {name}; no files were touched (a in the sidebar adds it back){}",
        saved_note(&saved)
    );
    match saved {
        Saved::Failed(_) => ed.set_status(Severity::Warning, msg),
        _ => ed.info(msg),
    }
    scan::request(ed);
}

// ----- typed commands ------------------------------------------------------

/// `:project-add [path]`: adds the project at `path`, or asks for one.
pub(crate) fn project_add_command(
    ed: &mut Editor,
    args: &[&str],
    _force: bool,
) -> Result<(), String> {
    if args.is_empty() {
        open_prompt(ed);
    } else {
        add(ed, &args.join(" "));
    }
    Ok(())
}

/// `:project-remove <name or path>`: forgets a project after a y/n
/// question.
pub(crate) fn project_remove_command(
    ed: &mut Editor,
    args: &[&str],
    _force: bool,
) -> Result<(), String> {
    if args.is_empty() {
        return Err(
            "give a project name or path (:project-remove <name>), or press d on its row in the \
             sidebar (space s)"
                .into(),
        );
    }
    if !ed.workspaces.scanned {
        scan::request(ed);
        return Err("the project list is still loading; try again in a moment".into());
    }
    let want = args.join(" ");
    let root = find(ed, &want)?;
    forget(ed, &root);
    Ok(())
}

/// The project named `want`, or the one holding the path `want`.
fn find(ed: &Editor, want: &str) -> Result<PathBuf, String> {
    let projects = &ed.workspaces.projects;
    let named: Vec<&Project> = projects.iter().filter(|p| p.name == want).collect();
    match named.as_slice() {
        [one] => return Ok(one.root.clone()),
        [] => {}
        many => {
            let paths: Vec<String> = many.iter().map(|p| p.root.display().to_string()).collect();
            return Err(format!(
                "{} projects are named {want}: {}; give the path instead",
                many.len(),
                paths.join(", ")
            ));
        }
    }
    let path = crate::editor::normalize_path(&ed.resolve_path(Path::new(want)));
    if let Some(p) = projects.iter().find(|p| p.root == path) {
        return Ok(p.root.clone());
    }
    // A path inside a worktree counts only when it exists, so a mistyped
    // name is not taken for a directory of the project you are in. The
    // deepest worktree holding it wins (worktrees can nest).
    if !path.exists() {
        return Err(format!(
            "no project named {want} or holding that path is in the list"
        ));
    }
    projects
        .iter()
        .flat_map(|p| p.worktrees.iter().map(move |w| (p, w)))
        .filter(|(_, w)| !w.missing && path.starts_with(&w.path))
        .max_by_key(|(_, w)| w.path.as_os_str().len())
        .map(|(p, _)| p.root.clone())
        .ok_or_else(|| format!("no project named {want} or holding that path is in the list"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_starts_at_the_parent_directory() {
        assert_eq!(prefill(Path::new("/")), "~/");
        assert_eq!(prefill(Path::new("/srv/code/app")), "/srv/code/");
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
            && home.is_absolute()
            && home.to_str().is_some()
        {
            assert_eq!(prefill(&home.join("src/app")), "~/src/");
            assert_eq!(prefill(&home.join("app")), "~/");
        }
    }

    #[test]
    fn adding_unhides_and_forgetting_hides() {
        let mut l = ProjectList::default();
        let a = Path::new("/w/a");
        let linked = Path::new("/w/a-feature");
        let belongs = belongs_to(a, &[a.to_path_buf(), linked.to_path_buf()]);
        l.forget(a, &belongs);
        assert_eq!(l.hidden, [a]);
        l.add(a, a);
        l.add(a, a);
        l.add(linked, a);
        assert_eq!(l.added, [a, linked]);
        assert!(l.hidden.is_empty());
        // Every entry of the project goes, the linked worktree's too.
        l.forget(a, &belongs);
        l.forget(a, &belongs);
        assert!(l.added.is_empty());
        assert_eq!(l.hidden, [a]);
        // The oldest hidden entries go first past the limit.
        for i in 0..MAX_ENTRIES {
            let p = PathBuf::from(format!("/h/{i}"));
            l.forget(&p, &|_| false);
        }
        assert_eq!(l.hidden.len(), MAX_ENTRIES);
        assert_eq!(l.hidden[0], Path::new("/h/0"));
    }

    #[test]
    fn unusable_files_are_refused_with_a_reason() {
        let dir = std::env::temp_dir().join(format!("maestro-projects-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(FILE);
        assert_eq!(read(&f), Ok(None));
        for (text, why) in [
            (&b"added = ["[..], "unclosed array"),
            (b"version = 2\nadded = []\n", "version 2 is not 1"),
            (b"version = 1\nadded = 3\n", "invalid type"),
            (b"\xff\xfe", "not valid UTF-8"),
        ] {
            std::fs::write(&f, text).unwrap();
            let got = read(&f);
            let err = got.clone().expect_err("refused");
            assert!(err.contains(why), "{text:?} gave {got:?}");
        }
        std::fs::write(&f, vec![b'#'; MAX_BYTES as usize + 1]).unwrap();
        assert_eq!(read(&f), Err("larger than 1 MiB".into()));
        std::fs::write(
            &f,
            "version = 1\nadded = [\"/w/a\", \"rel\", \"/w/a\"]\nunknown = true\n",
        )
        .unwrap();
        assert_eq!(
            read(&f),
            Ok(Some(ProjectList {
                added: vec![PathBuf::from("/w/a")],
                hidden: Vec::new(),
            }))
        );
        std::fs::remove_file(&f).unwrap();
        std::fs::create_dir(&f).unwrap();
        assert_eq!(read(&f), Err("not a regular file".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
