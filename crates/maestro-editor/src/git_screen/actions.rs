//! What the status screen's keys do. Each action works out its target on
//! the UI thread, then runs Git on a background thread and reloads the
//! screen when it is done.

use std::path::{Path, PathBuf};

use maestro_git::{ApplyTo, DiffSpec, FileChange, FilePatch, Picks, RemoteOp, Repository, Side};

use super::{BranchPicker, CommitMessage, Dialog, Page, Pending, Prepared, Row, Screen, Section};
use crate::editor::{Editor, Severity};
use crate::key::{KeyCode, KeyEvent};

/// The line that ends the message in a commit message buffer; it and
/// everything below it are left out, as with `git commit --verbose`.
pub const SCISSORS: &str = "# ------------------------ >8 ------------------------";

/// What `s`, `u` or `x` acts on.
#[derive(Debug, Clone)]
enum Target {
    Section(Section),
    Untracked(PathBuf),
    File(Section, usize),
    Hunk(Section, usize, usize),
    Lines(Section, usize, usize, Vec<usize>),
    Other,
}

fn target(sc: &Screen) -> Result<Target, String> {
    if let Some((from, to)) = sc.selection() {
        let mut found: Option<(Section, usize, usize)> = None;
        let mut lines = Vec::new();
        for r in &sc.rows[from..=to] {
            let Row::Line {
                section,
                file,
                hunk,
                line,
                ..
            } = r
            else {
                return Err("select lines inside one hunk".into());
            };
            match found {
                None => found = Some((*section, *file, *hunk)),
                Some(f) if f != (*section, *file, *hunk) => {
                    return Err("select lines inside one hunk".into());
                }
                _ => {}
            }
            lines.push(*line);
        }
        let (s, f, h) = found.ok_or("no lines selected")?;
        return Ok(Target::Lines(s, f, h, lines));
    }
    Ok(match sc.rows.get(sc.cursor) {
        Some(Row::Section { section, .. }) => Target::Section(*section),
        Some(Row::Untracked(i)) => sc
            .data
            .as_ref()
            .and_then(|d| d.untracked.get(*i))
            .cloned()
            .map_or(Target::Other, Target::Untracked),
        Some(Row::File { section, file }) => Target::File(*section, *file),
        Some(Row::Hunk {
            section,
            file,
            hunk,
            ..
        })
        | Some(Row::Line {
            section,
            file,
            hunk,
            ..
        }) => Target::Hunk(*section, *file, *hunk),
        _ => Target::Other,
    })
}

/// Refuses when another Git command from the screen is still running.
fn idle(ed: &mut Editor) -> Option<(Repository, PathBuf)> {
    let sc = ed.git.screen.as_ref()?;
    if let Some(b) = &sc.busy {
        let msg = format!("wait: {b} is still running");
        ed.set_status(Severity::Warning, msg);
        return None;
    }
    let Some(repo) = sc.repo.clone() else {
        ed.set_status(Severity::Warning, "the repository is still loading");
        return None;
    };
    let root = repo.root().to_path_buf();
    Some((repo, root))
}

/// Refuses a destructive action while a merge, rebase, cherry-pick,
/// revert or bisect is in progress, as the screen last saw it.
fn refuse_in_progress(ed: &mut Editor, what: &str) -> bool {
    let op = ed
        .git
        .screen
        .as_ref()
        .and_then(|s| s.data.as_ref())
        .and_then(|d| d.operation);
    if let Some(op) = op {
        ed.error(format!(
            "not {what}: a {} is in progress; finish it first ({})",
            op.label(),
            op.way_out()
        ));
        return true;
    }
    false
}

/// The same check, run again on the background thread just before acting.
fn check_no_operation(repo: &Repository, what: &str) -> Result<(), String> {
    match repo.operation().map_err(|e| e.to_string())? {
        Some(op) => Err(format!(
            "not {what}: a {} is in progress; finish it first ({})",
            op.label(),
            op.way_out()
        )),
        None => Ok(()),
    }
}

/// Runs `work` in the background with the screen marked busy, then shows
/// its message, tells the sign code the repository changed and reloads.
fn run_job<F>(ed: &mut Editor, what: &str, root: PathBuf, work: F)
where
    F: FnOnce() -> Result<String, String> + Send + 'static,
{
    let Some(sc) = &mut ed.git.screen else {
        return;
    };
    sc.busy = Some(what.to_string());
    let id = sc.id;
    let spawned = ed.spawn_background("git-screen-action", move |tx| {
        let result = work();
        tx.send(move |ed| {
            crate::git::repository_changed(ed, &root);
            match result {
                Ok(msg) => ed.info(msg),
                Err(e) => ed.error(e),
            }
            if let Some(sc) = &mut ed.git.screen
                && sc.id == id
            {
                sc.busy = None;
                super::reload(ed);
            }
        });
    });
    if let Err(e) = spawned {
        if let Some(sc) = &mut ed.git.screen {
            sc.busy = None;
        }
        ed.error(format!("could not start git: {e}"));
    }
}

/// Paths a file-level action names: both sides of a rename.
fn paths_of(f: &FilePatch) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(n) = &f.file.new_path {
        v.push(n.clone());
    }
    if let Some(o) = &f.file.old_path
        && Some(o) != f.file.new_path.as_ref()
    {
        v.push(o.clone());
    }
    v
}

/// True when a partial patch cannot express the change, so a whole-file
/// action stands in when every line is picked: an added or deleted file.
fn whole_file_only(f: &FilePatch) -> bool {
    // Renames, copies and mode changes are never undone by a hunk action:
    // their patches change only the contents of the file at its new path.
    matches!(f.file.change, FileChange::Added | FileChange::Deleted)
}

/// The picks for a hunk or line target.
fn picks_for(t: &Target, sc: &Screen) -> Option<(Section, usize, Picks, String)> {
    match t {
        Target::Hunk(s, f, h) => Some((*s, *f, Picks::new().hunk(*h), format!("hunk {}", h + 1))),
        Target::Lines(s, f, h, lines) => {
            let file = sc.file(*s, *f)?;
            let changed: Vec<usize> = lines
                .iter()
                .copied()
                .filter(|l| super::is_change(file, *h, *l))
                .collect();
            let n = changed.len();
            Some((
                *s,
                *f,
                Picks::new().lines(*h, changed),
                format!("{n} line(s) of hunk {}", h + 1),
            ))
        }
        _ => None,
    }
}

/// Re-reads `file`'s diff and fails unless it is byte for byte the diff
/// the screen drew, so a patch is only ever applied to what was shown.
fn check_unchanged(repo: &Repository, spec: &DiffSpec, file: &FilePatch) -> Result<(), String> {
    let paths = paths_of(file);
    let now = repo.file_patches(spec, &paths).map_err(|e| e.to_string())?;
    let same = now
        .iter()
        .find(|p| p.target_path() == file.target_path())
        .is_some_and(|p| p.raw == file.raw);
    if same {
        Ok(())
    } else {
        Err(format!(
            "{} changed since the screen was drawn; nothing was changed. r refreshes",
            file.target_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ))
    }
}

fn spec_of(s: Section) -> DiffSpec {
    if s == Section::Staged {
        DiffSpec::Staged
    } else {
        DiffSpec::Unstaged
    }
}

// ----- stage, unstage -----------------------------------------------

pub(super) fn stage(ed: &mut Editor) {
    let Some(sc) = &ed.git.screen else {
        return;
    };
    let t = match target(sc) {
        Ok(t) => t,
        Err(e) => return ed.error(e),
    };
    let Some(data) = sc.data.clone() else {
        return;
    };
    let picked = picks_for(&t, sc);
    let Some((repo, root)) = idle(ed) else {
        return;
    };
    match t {
        Target::Section(Section::Untracked) => {
            let paths = data.untracked.clone();
            run_job(ed, "staging", root, move || {
                let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
                repo.stage_paths(&refs).map_err(|e| e.to_string())?;
                Ok(format!("staged {} untracked file(s)", paths.len()))
            });
        }
        Target::Section(Section::Unstaged) => {
            if let Some(f) = data
                .unstaged
                .iter()
                .find(|f| f.file.unmerged && has_markers(&root, f))
            {
                return ed.error(format!(
                    "{} still has conflict markers (<<<<<<<); resolve them before staging",
                    f.target_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ));
            }
            let paths: Vec<PathBuf> = data.unstaged.iter().flat_map(paths_of).collect();
            run_job(ed, "staging", root, move || {
                let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
                repo.stage_paths(&refs).map_err(|e| e.to_string())?;
                Ok("staged every unstaged change".into())
            });
        }
        Target::Untracked(p) => {
            run_job(ed, "staging", root, move || {
                repo.stage_paths(&[p.as_path()])
                    .map_err(|e| e.to_string())?;
                Ok(format!("staged {}", p.display()))
            });
        }
        Target::File(Section::Unstaged, i) => {
            let f = data.unstaged[i].clone();
            if f.file.unmerged && has_markers(&root, &f) {
                return ed.error(
                    "the file still has conflict markers (<<<<<<<); resolve them before staging",
                );
            }
            run_job(ed, "staging", root, move || {
                let paths = paths_of(&f);
                let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
                repo.stage_paths(&refs).map_err(|e| e.to_string())?;
                Ok(format!("staged {}", super::file_label(&f).trim_start()))
            });
        }
        Target::Hunk(Section::Unstaged, ..) | Target::Lines(Section::Unstaged, ..) => {
            let Some((s, i, picks, what)) = picked else {
                return;
            };
            let f = data.files(s)[i].clone();
            patch_job(ed, repo, root, f, s, picks, what, Op::Stage);
        }
        Target::Section(Section::Staged)
        | Target::File(Section::Staged, _)
        | Target::Hunk(Section::Staged, ..)
        | Target::Lines(Section::Staged, ..) => ed.info("already staged; u unstages"),
        _ => ed.info("nothing to stage here"),
    }
}

pub(super) fn unstage(ed: &mut Editor) {
    let Some(sc) = &ed.git.screen else {
        return;
    };
    let t = match target(sc) {
        Ok(t) => t,
        Err(e) => return ed.error(e),
    };
    let Some(data) = sc.data.clone() else {
        return;
    };
    let picked = picks_for(&t, sc);
    let Some((repo, root)) = idle(ed) else {
        return;
    };
    match t {
        Target::Section(Section::Staged) => {
            let paths: Vec<PathBuf> = data.staged.iter().flat_map(paths_of).collect();
            run_job(ed, "unstaging", root, move || {
                let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
                repo.unstage_paths(&refs).map_err(|e| e.to_string())?;
                Ok("unstaged everything".into())
            });
        }
        Target::File(Section::Staged, i) => {
            let f = data.staged[i].clone();
            run_job(ed, "unstaging", root, move || {
                let paths = paths_of(&f);
                let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
                repo.unstage_paths(&refs).map_err(|e| e.to_string())?;
                Ok(format!("unstaged {}", super::file_label(&f).trim_start()))
            });
        }
        Target::Hunk(Section::Staged, ..) | Target::Lines(Section::Staged, ..) => {
            let Some((s, i, picks, what)) = picked else {
                return;
            };
            let f = data.files(s)[i].clone();
            patch_job(ed, repo, root, f, s, picks, what, Op::Unstage);
        }
        Target::Other | Target::Section(Section::Stashes | Section::Recent) => {
            ed.info("nothing to unstage here")
        }
        _ => ed.info("not staged; s stages"),
    }
}

/// Whether the working tree file still has conflict markers.
fn has_markers(root: &Path, f: &FilePatch) -> bool {
    let Some(p) = f.target_path() else {
        return false;
    };
    std::fs::read(root.join(p)).is_ok_and(|b| {
        b.split(|c| *c == b'\n')
            .any(|l| l.starts_with(b"<<<<<<< ") || l.starts_with(b">>>>>>> "))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Stage,
    Unstage,
    /// Discard from the working tree (an unstaged change).
    Discard,
    /// Discard from the index and the working tree (a staged change).
    DiscardStaged,
}

impl Op {
    fn verb(self) -> &'static str {
        match self {
            Op::Stage => "staged",
            Op::Unstage => "unstaged",
            Op::Discard | Op::DiscardStaged => "discarded",
        }
    }
}

/// Applies a hunk or line patch after checking the diff is unchanged.
#[allow(clippy::too_many_arguments)]
fn patch_job(
    ed: &mut Editor,
    repo: Repository,
    root: PathBuf,
    f: FilePatch,
    section: Section,
    picks: Picks,
    what: String,
    op: Op,
) {
    if picks.is_empty() {
        return ed.error("no changed line selected (context lines cannot be staged)");
    }
    let side = if op == Op::Stage {
        Side::Forward
    } else {
        Side::Reverse
    };
    let whole = whole_file_only(&f) && f.covers_all(&picks);
    let patch = if whole {
        None
    } else {
        match f.build(&picks, side) {
            Ok(p) => Some(p),
            Err(e) => return ed.error(e.to_string()),
        }
    };
    let busy = match op {
        Op::Stage => "staging",
        Op::Unstage => "unstaging",
        Op::Discard | Op::DiscardStaged => "discarding",
    };
    let name = f
        .target_path()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    run_job(ed, busy, root, move || {
        if matches!(op, Op::Discard | Op::DiscardStaged) {
            check_no_operation(&repo, "discarding")?;
        }
        check_unchanged(&repo, &spec_of(section), &f)?;
        if op == Op::DiscardStaged {
            check_no_unstaged(&repo, &f)?;
            check_no_untracked(&repo, &f)?;
        }
        let e = |e: maestro_git::GitError| e.to_string();
        let paths = paths_of(&f);
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        match (patch, op) {
            (None, Op::Stage) => repo.stage_paths(&refs).map_err(e)?,
            (None, Op::Unstage) => repo.unstage_paths(&refs).map_err(e)?,
            (None, Op::Discard) => repo.discard_paths(&refs).map_err(e)?,
            (None, Op::DiscardStaged) => repo.discard_staged_paths(&refs).map_err(e)?,
            (Some(p), Op::Stage) => repo.apply_patch(&p, ApplyTo::Index, false).map_err(e)?,
            (Some(p), Op::Unstage) => repo.apply_patch(&p, ApplyTo::Index, true).map_err(e)?,
            (Some(p), Op::Discard) => repo.apply_patch(&p, ApplyTo::Worktree, true).map_err(e)?,
            (Some(p), Op::DiscardStaged) => repo.apply_patch(&p, ApplyTo::Both, true).map_err(e)?,
        }
        Ok(format!("{} {what} of {name}", op.verb()))
    });
}

/// Fails when `f` (a staged file) also has unstaged changes, which a
/// discard from both the index and the working tree would lose too.
fn check_no_unstaged(repo: &Repository, f: &FilePatch) -> Result<(), String> {
    let now = repo
        .file_patches(&DiffSpec::Unstaged, &paths_of(f))
        .map_err(|e| e.to_string())?;
    if now.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} has unstaged changes now; discard or stage them first. Nothing was changed",
            f.target_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ))
    }
}

/// Fails when a path a staged discard would restore from `HEAD` now holds
/// an untracked file (say, a file recreated after its deletion was
/// staged): restoring would overwrite it.
fn check_no_untracked(repo: &Repository, f: &FilePatch) -> Result<(), String> {
    for p in paths_of(f) {
        let full = repo.root().join(&p);
        if std::fs::symlink_metadata(&full).is_ok()
            && repo.is_untracked(&full).map_err(|e| e.to_string())?
        {
            return Err(format!(
                "{} is an untracked file now and discarding would overwrite it; nothing was changed",
                p.display()
            ));
        }
    }
    Ok(())
}

// ----- discard ----------------------------------------------------------

/// A discard waiting for confirmation.
#[derive(Debug, Clone)]
pub struct Discard {
    kind: DiscardKind,
}

#[derive(Debug, Clone)]
enum DiscardKind {
    Untracked {
        path: PathBuf,
        len: u64,
        modified: Option<std::time::SystemTime>,
    },
    File {
        section: Section,
        file: Box<FilePatch>,
    },
    Patch {
        section: Section,
        file: Box<FilePatch>,
        picks: Picks,
        what: String,
    },
}

pub(super) fn discard(ed: &mut Editor) {
    if refuse_in_progress(ed, "discarding") {
        return;
    }
    let Some(sc) = &ed.git.screen else {
        return;
    };
    let t = match target(sc) {
        Ok(t) => t,
        Err(e) => return ed.error(e),
    };
    let Some(data) = sc.data.clone() else {
        return;
    };
    let picked = picks_for(&t, sc);
    let Some((_, root)) = idle(ed) else {
        return;
    };
    let open_buffer = |ed: &Editor, rel: &Path| -> Option<bool> {
        let id = ed.find_buffer_by_path(&root.join(rel))?;
        Some(ed.buffers[&id].doc.is_modified())
    };
    let (title, mut lines, kind) = match t {
        Target::Untracked(path) => {
            let meta = std::fs::symlink_metadata(root.join(&path));
            let (len, modified) = match &meta {
                Ok(m) => (m.len(), m.modified().ok()),
                Err(e) => return ed.error(format!("{}: {e}", path.display())),
            };
            let lines = vec![
                format!(
                    "Deletes the untracked file {} ({len} bytes).",
                    path.display()
                ),
                "Git never stored it, so Git cannot bring it back.".to_string(),
            ];
            (
                format!("delete {}", path.display()),
                lines,
                DiscardKind::Untracked {
                    path,
                    len,
                    modified,
                },
            )
        }
        Target::File(section @ (Section::Unstaged | Section::Staged), i) => {
            let f = data.files(section)[i].clone();
            if f.file.unmerged {
                return ed.error("the file is in conflict; resolve it instead of discarding");
            }
            let name = f.target_path().cloned().unwrap_or_default();
            let (add, del) = f.file.counts();
            let mut lines = Vec::new();
            if section == Section::Unstaged {
                lines.push(format!(
                    "Puts {} back to its staged version, losing every unstaged change to it ({add} added and {del} removed line(s){}).",
                    name.display(),
                    if f.file.binary { ", binary" } else { "" }
                ));
                lines.push(
                    "Unstaged changes are in no commit and not in the index, so Git cannot bring them back."
                        .into(),
                );
            } else {
                if data
                    .unstaged
                    .iter()
                    .any(|u| u.target_path() == f.target_path())
                {
                    return ed.error(format!(
                        "{} also has unstaged changes, which this would lose too; discard or stage those first",
                        name.display()
                    ));
                }
                let what = match f.file.change {
                    FileChange::Added => {
                        "Deletes it from the index and the working tree".to_string()
                    }
                    FileChange::Renamed { .. } => {
                        "Undoes the rename in the index and the working tree".to_string()
                    }
                    _ => "Puts it back to the last commit in the index and the working tree"
                        .to_string(),
                };
                lines.push(format!(
                    "{what}: {} ({add} added and {del} removed line(s)).",
                    super::file_label(&f).trim_start()
                ));
                lines.push(
                    "The staged text stays in Git's object store only until garbage collection; git fsck --lost-found can find it until then."
                        .into(),
                );
            }
            (
                format!("discard {}", name.display()),
                lines,
                DiscardKind::File {
                    section,
                    file: Box::new(f),
                },
            )
        }
        Target::Hunk(..) | Target::Lines(..) => {
            let Some((section, i, picks, what)) = picked else {
                return;
            };
            if section == Section::Untracked {
                return;
            }
            let f = data.files(section)[i].clone();
            let name = f.target_path().cloned().unwrap_or_default();
            if section == Section::Staged
                && data
                    .unstaged
                    .iter()
                    .any(|u| u.target_path() == f.target_path())
            {
                return ed.error(format!(
                    "{} also has unstaged changes; discard or stage them first, then discard the staged change",
                    name.display()
                ));
            }
            if picks.is_empty() {
                return ed.error("no changed line selected");
            }
            if let Err(e) = f.build(&picks, Side::Reverse) {
                return ed.error(e.to_string());
            }
            let (add, del) = selected_counts(&f, &picks);
            let mut lines = vec![format!(
                "Discards {what} of {}: {add} added line(s) are removed and {del} removed line(s) come back, {}.",
                name.display(),
                if section == Section::Staged {
                    "in the index and the working tree"
                } else {
                    "in the working tree"
                }
            )];
            if whole_file_only(&f) && f.covers_all(&picks) {
                lines.push(match f.file.change {
                    FileChange::Added => format!(
                        "That is every line of the new file, so {} itself is removed{}.",
                        name.display(),
                        if section == Section::Staged {
                            " from the index and the working tree"
                        } else {
                            ""
                        }
                    ),
                    _ => format!(
                        "That is every line of the deleted file, so {} comes back whole.",
                        name.display()
                    ),
                });
            }
            lines.push(if section == Section::Staged {
                "The staged text stays in Git's object store only until garbage collection; git fsck --lost-found can find it until then.".into()
            } else {
                "Unstaged changes are in no commit and not in the index, so Git cannot bring them back.".into()
            });
            (
                format!("discard {what} of {}", name.display()),
                lines,
                DiscardKind::Patch {
                    section,
                    file: Box::new(f),
                    picks,
                    what,
                },
            )
        }
        _ => return ed.info("nothing to discard here"),
    };
    let rel = match &kind {
        DiscardKind::Untracked { path, .. } => Some(path.clone()),
        DiscardKind::File { file, .. } | DiscardKind::Patch { file, .. } => {
            file.target_path().cloned()
        }
    };
    match rel.as_deref().and_then(|r| open_buffer(ed, r)) {
        Some(false) => lines.push(
            "The file is open in a buffer without unsaved edits: it reloads, and u in that buffer brings the old text back."
                .into(),
        ),
        Some(true) => lines.push(
            "The file is open in a buffer with unsaved edits; the buffer keeps them and will say the file changed on disk."
                .into(),
        ),
        None => {}
    }
    if let Some(sc) = &mut ed.git.screen {
        sc.dialog = Some(Dialog {
            title,
            lines,
            keys: vec![
                ("y".into(), "discard".into()),
                ("n".into(), "cancel".into()),
            ],
            pending: Pending::Discard(Discard { kind }),
        });
    }
}

/// Lines added and removed by the picked lines.
fn selected_counts(f: &FilePatch, picks: &Picks) -> (usize, usize) {
    // Build the patch forward and count its markers: exact, whatever the
    // pick holds.
    let Ok(patch) = f.build(picks, Side::Forward) else {
        return f.file.counts();
    };
    let text = String::from_utf8_lossy(&patch);
    let mut add = 0;
    let mut del = 0;
    let mut in_hunk = false;
    for l in text.lines() {
        if l.starts_with("@@ ") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if l.starts_with('+') {
            add += 1;
        } else if l.starts_with('-') {
            del += 1;
        }
    }
    (add, del)
}

fn run_discard(ed: &mut Editor, d: Discard) {
    let Some((repo, root)) = idle(ed) else {
        return;
    };
    match d.kind {
        DiscardKind::Untracked {
            path,
            len,
            modified,
        } => run_job(ed, "deleting", root.clone(), move || {
            check_no_operation(&repo, "discarding")?;
            let meta = std::fs::symlink_metadata(root.join(&path)).map_err(|e| e.to_string())?;
            if meta.len() != len || meta.modified().ok() != modified {
                return Err(format!(
                    "{} changed since you were asked; nothing was deleted",
                    path.display()
                ));
            }
            repo.remove_untracked(&path).map_err(|e| e.to_string())?;
            Ok(format!("deleted {}", path.display()))
        }),
        DiscardKind::File { section, file } => {
            let f = *file;
            run_job(ed, "discarding", root, move || {
                check_no_operation(&repo, "discarding")?;
                check_unchanged(&repo, &spec_of(section), &f)?;
                let paths = paths_of(&f);
                let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
                let e = |e: maestro_git::GitError| e.to_string();
                if section == Section::Staged {
                    check_no_unstaged(&repo, &f)?;
                    check_no_untracked(&repo, &f)?;
                    repo.discard_staged_paths(&refs).map_err(e)?;
                } else {
                    repo.discard_paths(&refs).map_err(e)?;
                }
                Ok(format!(
                    "discarded {}",
                    f.target_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ))
            });
        }
        DiscardKind::Patch {
            section,
            file,
            picks,
            what,
        } => {
            let op = if section == Section::Staged {
                Op::DiscardStaged
            } else {
                Op::Discard
            };
            patch_job(ed, repo, root, *file, section, picks, what, op);
        }
    }
}

/// Keys while a confirmation dialog is up.
pub(super) fn dialog_key(ed: &mut Editor, key: KeyEvent) {
    let Some(sc) = &mut ed.git.screen else {
        return;
    };
    let Some(d) = sc.dialog.take() else {
        return;
    };
    let ch = key.typed_char();
    if key.code == KeyCode::Esc || ch == Some('n') || ch == Some('q') {
        ed.info(format!("{}: canceled, nothing changed", d.title));
        return;
    }
    if ch != Some('y') {
        sc.dialog = Some(d);
        return;
    }
    if !crate::agents::panel::dialog_fits(ed.area, &d.lines, &d.keys) {
        ed.error("the window is too small to show this whole dialog; make it larger, or press n");
        if let Some(sc) = &mut ed.git.screen {
            sc.dialog = Some(d);
        }
        return;
    }
    match d.pending {
        Pending::Discard(x) => run_discard(ed, x),
        Pending::Amend(prepared) => open_message(ed, true, prepared),
        Pending::Remote(cmd) => run_remote(ed, cmd),
    }
}

// ----- commit -----------------------------------------------------------

pub(super) fn commit(ed: &mut Editor, amend: bool) {
    let Some(sc) = &ed.git.screen else {
        return;
    };
    let Some(data) = sc.data.clone() else {
        return;
    };
    if ed.git.commit_message.is_some() {
        return ed.error("a commit message is already being written; save and close it first");
    }
    if amend {
        if refuse_in_progress(ed, "amending") {
            return;
        }
        if data.head.is_none() {
            return ed.error("there is no commit to amend yet");
        }
    } else if data.staged.is_empty() && data.operation != Some(maestro_git::Operation::Merge) {
        return ed
            .error("nothing staged to commit; s stages the file, hunk or lines under the cursor");
    }
    let Some((repo, _)) = idle(ed) else {
        return;
    };
    if let Some(sc) = &mut ed.git.screen {
        sc.busy = Some("preparing the commit message".into());
    }
    let id = ed.git.screen.as_ref().map(|s| s.id);
    let spawned = ed.spawn_background("git-commit-prepare", move |tx| {
        let result = (|| {
            let e = |e: maestro_git::GitError| e.to_string();
            let mut p = Prepared {
                stat: repo.staged_stat().map_err(e)?,
                head: if repo.has_head().map_err(e)? {
                    Some(repo.resolve("HEAD").map_err(e)?)
                } else {
                    None
                },
                branch: repo.head().map_err(e)?.branch().map(str::to_string),
                operation: repo.operation().map_err(e)?,
                ..Default::default()
            };
            if !amend {
                return Ok((p, Vec::new()));
            }
            if let Some(op) = p.operation {
                return Err(format!(
                    "not amending: a {} is in progress; finish it first ({})",
                    op.label(),
                    op.way_out()
                ));
            }
            p.message = repo.head_message().map_err(e)?;
            let remotes = repo.head_on_remotes().map_err(e)?;
            Ok::<_, String>((p, remotes))
        })();
        tx.send(move |ed| {
            let Some(sc) = &mut ed.git.screen else {
                return;
            };
            if Some(sc.id) != id {
                return;
            }
            sc.busy = None;
            match result {
                Err(e) => ed.error(e),
                Ok((prepared, remotes)) if !remotes.is_empty() => {
                    let head = sc
                        .data
                        .as_ref()
                        .and_then(|d| d.head.clone())
                        .unwrap_or_default();
                    sc.dialog = Some(Dialog {
                        title: "amend a pushed commit".into(),
                        lines: vec![
                            format!(
                                "HEAD ({head}) is already on {}.",
                                remotes.join(", ")
                            ),
                            "Amending replaces it with a new commit. Anyone who has the old one keeps it, and pushing the new one would need a force push, which Maestro never does."
                                .into(),
                        ],
                        keys: vec![
                            ("y".into(), "amend anyway".into()),
                            ("n".into(), "cancel".into()),
                        ],
                        pending: Pending::Amend(prepared),
                    });
                }
                Ok((prepared, _)) => open_message(ed, amend, prepared),
            }
        });
    });
    if let Err(e) = spawned {
        if let Some(sc) = &mut ed.git.screen {
            sc.busy = None;
        }
        ed.error(format!("could not start git: {e}"));
    }
}

/// The text a commit message buffer starts with.
pub fn template(amend: bool, message: &str, stat: &str) -> String {
    let mut t = String::new();
    if amend {
        t.push_str(message.trim_end());
        t.push('\n');
    } else {
        t.push('\n');
    }
    t.push_str(if amend {
        "# Amending HEAD: edit the message above.\n"
    } else {
        "# Write the commit message above.\n"
    });
    t.push_str("# Save and close this buffer (:wq) to commit; an empty message aborts.\n");
    t.push_str("# Lines starting with # are left out.\n");
    t.push_str(SCISSORS);
    t.push('\n');
    t.push_str("# Everything below the line above is left out of the message.\n");
    if stat.trim().is_empty() {
        t.push_str("Nothing staged.\n");
    } else {
        t.push_str("Staged:\n");
        t.push_str(stat);
        if !stat.ends_with('\n') {
            t.push('\n');
        }
    }
    t
}

/// The message in a commit message buffer: the text above the scissors
/// line, without `#` lines, trailing spaces, or blank lines at either end
/// (several blank lines in a row become one). Empty means abort.
pub fn clean_message(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim_end() == SCISSORS {
            break;
        }
        if line.starts_with('#') {
            continue;
        }
        let line = line.trim_end();
        if line.is_empty() && out.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    if out.is_empty() {
        return String::new();
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// A new file for one commit message in the Git directory, so two
/// editors (or two messages) never share one.
fn message_file(git_dir: &Path, text: &str) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = git_dir.join(format!("MAESTRO_EDITMSG-{}-{n}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(text.as_bytes())?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

fn open_message(ed: &mut Editor, amend: bool, prepared: Prepared) {
    let Some(repo) = ed.git.screen.as_ref().and_then(|s| s.repo.clone()) else {
        return;
    };
    let text = template(amend, &prepared.message, &prepared.stat);
    let path = match message_file(repo.git_dir(), &text) {
        Ok(p) => p,
        Err(e) => {
            return ed.error(format!(
                "could not write a message file in {}: {e}",
                ed.show_path(repo.git_dir())
            ));
        }
    };
    let written = maestro_core::io::DiskState::read(&path);
    ed.git.screen = None;
    ed.split(crate::layout::Direction::Horizontal);
    let doc = match ed.open(&path) {
        Ok(d) => d,
        Err(e) => {
            ed.close_view();
            let _ = std::fs::remove_file(&path);
            return ed.error(e);
        }
    };
    ed.set_selection(maestro_core::Selection::point(0));
    ed.git.commit_message = Some(CommitMessage {
        doc,
        path,
        repo,
        amend,
        written,
        dirty: false,
        prepared,
    });
    ed.info(if amend {
        "edit the message, then :wq amends; closing without saving, or with an empty message, aborts"
    } else {
        "write the message, then :wq commits; closing without saving, or with an empty message, aborts"
    });
}

/// Commits once the message buffer is no longer shown in any view, with
/// the text saved to its file. Nothing is committed when the buffer was
/// never saved, had unsaved edits when it went away, or holds an empty
/// message, or when `HEAD`, the branch or the operation in progress
/// changed since the message was opened. Called from
/// [`crate::git::refresh`].
pub fn check_commit_message(ed: &mut Editor) {
    let Some(cm) = &mut ed.git.commit_message else {
        return;
    };
    let buffer = ed.buffers.get(&cm.doc);
    if let Some(b) = buffer {
        cm.dirty = b.doc.is_modified();
        if ed.views.values().any(|v| v.doc == cm.doc) {
            return;
        }
    }
    let Some(cm) = ed.git.commit_message.take() else {
        return;
    };
    if ed.buffers.contains_key(&cm.doc) {
        let _ = ed.close_buffer(cm.doc, true);
    }
    let what = if cm.amend { "amend" } else { "commit" };
    let saved = match (cm.written, maestro_core::io::DiskState::read(&cm.path)) {
        (Some(w), Some(now)) => w.metadata_differs(&now),
        _ => false,
    };
    if cm.dirty || !saved {
        let _ = std::fs::remove_file(&cm.path);
        ed.set_status(
            Severity::Warning,
            format!("{what} aborted: the message buffer was closed without saving"),
        );
        return;
    }
    let text = std::fs::read_to_string(&cm.path).unwrap_or_default();
    let message = clean_message(&text);
    if message.is_empty() {
        let _ = std::fs::remove_file(&cm.path);
        ed.set_status(Severity::Warning, format!("{what} aborted: empty message"));
        return;
    }
    let root = cm.repo.root().to_path_buf();
    let repo = cm.repo.clone();
    let path = cm.path.clone();
    let amend = cm.amend;
    let prepared = cm.prepared.clone();
    let spawned = ed.spawn_background("git-commit", move |tx| {
        let result = (|| {
            let e = |e: maestro_git::GitError| e.to_string();
            let head = if repo.has_head().map_err(e)? {
                Some(repo.resolve("HEAD").map_err(e)?)
            } else {
                None
            };
            let branch = repo.head().map_err(e)?.branch().map(str::to_string);
            let operation = repo.operation().map_err(e)?;
            if head != prepared.head || branch != prepared.branch {
                return Err("HEAD moved (a commit, or another branch checked out) since the message was opened".to_string());
            }
            if operation != prepared.operation {
                return Err(match operation {
                    Some(op) => format!("a {} started since the message was opened", op.label()),
                    None => "the operation in progress ended since the message was opened".into(),
                });
            }
            repo.commit(&message, amend).map_err(e)
        })();
        let subject = message.lines().next().unwrap_or_default().to_string();
        tx.send(move |ed| {
            crate::git::repository_changed(ed, &root);
            match result {
                Ok(id) => {
                    let _ = std::fs::remove_file(&path);
                    ed.info(format!(
                        "{} {}: {subject}",
                        if amend { "amended" } else { "committed" },
                        &id[..id.len().min(7)]
                    ));
                    if ed.git.screen.is_none() {
                        super::open_at(ed, root);
                    }
                }
                Err(e) => {
                    let kept = ed.show_path(&path);
                    ed.error(format!(
                        "not {}: {e}; the message is kept in {kept}",
                        if amend { "amended" } else { "committed" },
                    ))
                }
            }
        });
    });
    if let Err(e) = spawned {
        ed.error(format!("could not start git commit: {e}"));
    }
}

// ----- branches -----------------------------------------------------------

pub(super) fn branch(ed: &mut Editor) {
    if refuse_in_progress(ed, "switching branches") {
        return;
    }
    let Some((repo, _)) = idle(ed) else {
        return;
    };
    let id = ed.git.screen.as_ref().map(|s| s.id);
    if let Some(sc) = &mut ed.git.screen {
        sc.busy = Some("listing branches".into());
    }
    let spawned = ed.spawn_background("git-branches", move |tx| {
        let result = repo.branches().map_err(|e| e.to_string());
        tx.send(move |ed| {
            let Some(sc) = &mut ed.git.screen else {
                return;
            };
            if Some(sc.id) != id {
                return;
            }
            sc.busy = None;
            match result {
                Ok(branches) => {
                    let mut p = BranchPicker {
                        branches,
                        ..Default::default()
                    };
                    p.refilter();
                    sc.branches = Some(p);
                }
                Err(e) => ed.error(e),
            }
        });
    });
    if let Err(e) = spawned {
        ed.error(format!("could not start git: {e}"));
    }
}

pub(super) fn branch_key(ed: &mut Editor, key: KeyEvent) {
    let Some(sc) = &mut ed.git.screen else {
        return;
    };
    let Some(p) = &mut sc.branches else {
        return;
    };
    let ctrl = key.mods.ctrl;
    match key.code {
        KeyCode::Esc => {
            sc.branches = None;
        }
        KeyCode::Char('c') if ctrl => {
            sc.branches = None;
        }
        KeyCode::Up => p.selected = p.selected.saturating_sub(1),
        KeyCode::Char('p') | KeyCode::Char('k') if ctrl => {
            p.selected = p.selected.saturating_sub(1)
        }
        KeyCode::Down => p.selected = (p.selected + 1).min(p.items().saturating_sub(1)),
        KeyCode::Char('n') | KeyCode::Char('j') if ctrl => {
            p.selected = (p.selected + 1).min(p.items().saturating_sub(1))
        }
        KeyCode::Backspace => {
            p.query.pop();
            p.refilter();
        }
        KeyCode::Enter => {
            let choice = if p.selected < p.matches.len() {
                Some((p.branches[p.matches[p.selected]].clone(), false))
            } else if p.offers_create() {
                Some((
                    maestro_git::Branch {
                        name: p.query.trim().to_string(),
                        current: false,
                        upstream: None,
                        subject: String::new(),
                    },
                    true,
                ))
            } else {
                None
            };
            sc.branches = None;
            let Some((b, create)) = choice else {
                return;
            };
            if b.current {
                return ed.info(format!("already on {}", b.name));
            }
            let Some((repo, root)) = idle(ed) else {
                return;
            };
            let name = b.name;
            run_job(ed, "switching branches", root, move || {
                check_no_operation(&repo, "switching branches")?;
                let e = |e: maestro_git::GitError| e.to_string();
                if create {
                    repo.create_branch(&name).map_err(e)?;
                    Ok(format!("created and switched to {name}"))
                } else {
                    repo.switch_branch(&name).map_err(e)?;
                    Ok(format!("switched to {name}"))
                }
            });
        }
        _ => {
            if let Some(c) = key.typed_char() {
                p.query.push(c);
                p.selected = 0;
                p.refilter();
            }
        }
    }
}

// ----- stash ----------------------------------------------------------------

pub(super) fn stash(ed: &mut Editor) {
    if refuse_in_progress(ed, "stashing") {
        return;
    }
    let Some((repo, root)) = idle(ed) else {
        return;
    };
    run_job(ed, "stashing", root, move || {
        check_no_operation(&repo, "stashing")?;
        if repo.stash_push(None).map_err(|e| e.to_string())? {
            Ok("stashed staged and unstaged changes to tracked files (Z pops)".into())
        } else {
            Ok("nothing to stash (untracked files are not stashed)".into())
        }
    });
}

pub(super) fn pop(ed: &mut Editor) {
    if refuse_in_progress(ed, "popping a stash") {
        return;
    }
    let Some(sc) = &ed.git.screen else {
        return;
    };
    let Some(data) = &sc.data else {
        return;
    };
    let chosen = match sc.rows.get(sc.cursor) {
        Some(Row::Stash(i)) => data.stashes.get(*i).cloned(),
        _ => data.stashes.first().cloned(),
    };
    let Some(s) = chosen else {
        return ed.info("no stash to pop");
    };
    let Some((repo, root)) = idle(ed) else {
        return;
    };
    run_job(ed, "popping the stash", root, move || {
        check_no_operation(&repo, "popping a stash")?;
        repo.stash_pop(s.index, &s.id).map_err(|e| e.to_string())?;
        Ok(format!("popped stash@{{{}}}: {}", s.index, s.message))
    });
}

// ----- remotes ------------------------------------------------------------

pub(super) fn remote(ed: &mut Editor, op: RemoteOp) {
    if op == RemoteOp::Pull && refuse_in_progress(ed, "pulling") {
        return;
    }
    let Some((repo, _)) = idle(ed) else {
        return;
    };
    let id = ed.git.screen.as_ref().map(|s| s.id);
    if let Some(sc) = &mut ed.git.screen {
        sc.busy = Some(format!("preparing {}", op.label()));
    }
    let spawned = ed.spawn_background("git-remote-plan", move |tx| {
        let result = repo.remote_command(op).map_err(|e| e.to_string());
        tx.send(move |ed| {
            let Some(sc) = &mut ed.git.screen else {
                return;
            };
            if Some(sc.id) != id {
                return;
            }
            sc.busy = None;
            match result {
                Err(e) => ed.error(e),
                Ok(cmd) => {
                    let mut lines = vec![
                        format!("Runs: {}", cmd.display()),
                        format!("Remote {}: {}", cmd.remote, cmd.url),
                    ];
                    lines.push(match op {
                        RemoteOp::Fetch => "Updates remote-tracking branches only; your branches and files do not change.".into(),
                        RemoteOp::Pull => "Fast-forward only: if your branch and the remote have both moved, nothing changes and git says so.".into(),
                        RemoteOp::Push => "Never forces: if the remote has commits you do not, git refuses and nothing is overwritten.".into(),
                    });
                    sc.dialog = Some(Dialog {
                        title: format!("{} {}", op.label(), cmd.remote),
                        lines,
                        keys: vec![
                            ("y".into(), op.label().into()),
                            ("n".into(), "cancel".into()),
                        ],
                        pending: Pending::Remote(cmd),
                    });
                }
            }
        });
    });
    if let Err(e) = spawned {
        ed.error(format!("could not start git: {e}"));
    }
}

fn run_remote(ed: &mut Editor, cmd: maestro_git::RemoteCommand) {
    let Some((repo, root)) = idle(ed) else {
        return;
    };
    let busy = format!("{} {}", cmd.op.label(), cmd.remote);
    run_job(ed, &busy, root, move || {
        if cmd.op == RemoteOp::Pull {
            check_no_operation(&repo, "pulling")?;
        }
        let out = repo.run_remote(&cmd).map_err(|e| e.to_string())?;
        let last = out.lines().last().unwrap_or_default().trim().to_string();
        Ok(if last.is_empty() {
            format!("{} done", cmd.display())
        } else {
            format!("{} done: {last}", cmd.display())
        })
    });
}

// ----- log ------------------------------------------------------------------

pub(super) fn log(ed: &mut Editor) {
    let Some((repo, _)) = idle(ed) else {
        return;
    };
    let id = ed.git.screen.as_ref().map(|s| s.id);
    if let Some(sc) = &mut ed.git.screen {
        sc.busy = Some("reading the log".into());
    }
    let spawned = ed.spawn_background("git-log", move |tx| {
        let result = repo.log(500, true).map_err(|e| e.to_string());
        tx.send(move |ed| {
            let Some(sc) = &mut ed.git.screen else {
                return;
            };
            if Some(sc.id) != id {
                return;
            }
            sc.busy = None;
            match result {
                Ok(entries) if entries.is_empty() => ed.info("no commits yet"),
                Ok(entries) => sc.pages.push(Page::Log {
                    entries,
                    cursor: 0,
                    top: 0,
                }),
                Err(e) => ed.error(e),
            }
        });
    });
    if let Err(e) = spawned {
        ed.error(format!("could not start git: {e}"));
    }
}

pub(super) fn show_commit(ed: &mut Editor, rev: String) {
    let Some((repo, _)) = idle(ed) else {
        return;
    };
    let id = ed.git.screen.as_ref().map(|s| s.id);
    if let Some(sc) = &mut ed.git.screen {
        sc.busy = Some("reading the commit".into());
    }
    let spawned = ed.spawn_background("git-show", move |tx| {
        let result = repo.commit_detail(&rev).map_err(|e| e.to_string());
        tx.send(move |ed| {
            let Some(sc) = &mut ed.git.screen else {
                return;
            };
            if Some(sc.id) != id {
                return;
            }
            sc.busy = None;
            match result {
                Ok(detail) => {
                    let rows = super::detail_rows(&detail);
                    sc.pages.push(Page::Commit {
                        detail: Box::new(detail),
                        rows,
                        cursor: 0,
                        top: 0,
                    });
                }
                Err(e) => ed.error(e),
            }
        });
    });
    if let Err(e) = spawned {
        ed.error(format!("could not start git: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_cleaned_like_git_strip() {
        let t = template(false, "", " a | 1 +\n");
        assert_eq!(clean_message(&t), "");
        let text = format!("\n\nSubject  \n\n\n# comment\nBody line\n\n{SCISSORS}\nnot this\n");
        assert_eq!(clean_message(&text), "Subject\n\nBody line\n");
        let amend = template(true, "Old subject\n\nOld body\n", "");
        assert_eq!(clean_message(&amend), "Old subject\n\nOld body\n");
        assert!(amend.contains("Nothing staged."));
    }
}
