//! What the inbox does: opening the browser, checking a pull request out
//! into a worktree, and the write actions on GitHub. A write action never
//! runs on a key alone: a dialog first shows the exact `gh` command and
//! any text that goes to its standard input, and `y` sends it.

use std::path::{Path, PathBuf};

use maestro_forge::{
    CheckState, MergeMethod, Mergeable, PrRef, PrStatus, ReviewDecision, WriteAction,
};

use super::{DraftKind, MessageDraft};
use crate::editor::{DocumentId, Editor, Mode};
use crate::key::{KeyCode, KeyEvent};
use maestro_core::DocumentVersion;

/// A message buffer and the version its text was taken at.
pub type Draft = (DocumentId, DocumentVersion);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// `y` sends `action` to `pr`.
    Confirm {
        title: String,
        lines: Vec<String>,
        keys: Vec<(String, String)>,
        pr: PrRef,
        action: WriteAction,
        /// The message buffer the body came from and its version then.
        /// Closed once sent, unless it changed since.
        draft: Option<Draft>,
    },
    /// Choosing how to merge `pr`.
    Merge {
        pr: PrRef,
        title: String,
        status: PrStatus,
        base: Option<String>,
        selected: usize,
    },
}

/// Control and direction-changing characters, shown escaped so text from
/// GitHub or a buffer cannot hide part of a dialog line.
fn visible(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                c.escape_unicode().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn confirm_keys() -> Vec<(String, String)> {
    vec![
        ("y".into(), "send to GitHub".into()),
        ("n/esc".into(), "cancel".into()),
    ]
}

/// The lines every write dialog ends with: the text as it will read, the
/// command, and exactly what goes to its standard input.
fn command_lines(pr: &PrRef, action: &WriteAction) -> Vec<String> {
    let cmd = action.command(pr);
    let mut lines = Vec::new();
    let body = action.body();
    let stdin = cmd.stdin.as_deref();
    let n_lines = |s: &str| {
        let n = s.lines().count().max(1);
        format!("{n} line{}", if n == 1 { "" } else { "s" })
    };
    // A review's text is inside JSON; show it as it will read first.
    if let Some(b) = body
        && stdin != Some(b)
    {
        lines.push(format!("The text ({}):", n_lines(b)));
        lines.extend(b.lines().map(|l| format!("  │ {}", visible(l))));
    }
    lines.push("This runs:".into());
    lines.push(format!("  {}", visible(&cmd.display())));
    if let Some(input) = stdin {
        lines.push(format!(
            "with this on its standard input ({}):",
            n_lines(input)
        ));
        lines.extend(input.lines().map(|l| format!("  │ {}", visible(l))));
    }
    lines
}

/// `abc123456789 (the head shown here)` for a review or merge dialog.
fn pinned_line(action: &WriteAction) -> Option<String> {
    let head = action.head()?;
    let short = maestro_forge::short(head);
    Some(match action {
        WriteAction::Merge { .. } => format!(
            "Only head commit {short} is merged: if commits were pushed since this screen \
             loaded, GitHub refuses."
        ),
        _ => format!(
            "The review is on commit {short}, the head shown here. If newer commits were \
             pushed since, nothing is sent."
        ),
    })
}

fn ask(ed: &mut Editor, pr: PrRef, title: &str, action: WriteAction, draft: Option<Draft>) {
    if let Some(w) = &ed.forge.writing {
        ed.error(format!("still sending: {w}; wait for it to finish"));
        return;
    }
    let mut lines = vec![format!(
        "{} {pr} on GitHub, as the account gh is logged in to.",
        capitalize(action.verb())
    )];
    if !title.is_empty() {
        lines.push(format!("\"{}\"", visible(title)));
    }
    match &action {
        WriteAction::Comment { .. } | WriteAction::RequestChanges { .. } => lines.push(
            "Everyone who can see the pull request can read it, and GitHub may notify its \
             participants."
                .into(),
        ),
        WriteAction::Approve { .. } => lines.push("The review is submitted with no text.".into()),
        WriteAction::Merge { .. } => {}
    }
    lines.extend(pinned_line(&action));
    lines.extend(command_lines(&pr, &action));
    ed.forge.screen.dialog = Some(Dialog::Confirm {
        title: format!("{} {pr}", action.verb()),
        lines,
        keys: confirm_keys(),
        pr,
        action,
        draft,
    });
    ed.forge.open = true;
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// The head commit of `pr` as shown on screen, or why there is none.
fn shown_head(ed: &Editor, pr: &PrRef) -> Result<String, String> {
    match known_status(ed, pr) {
        Some((s, _)) if !s.head_oid.is_empty() => Ok(s.head_oid),
        Some(_) => Err(format!(
            "GitHub did not give the head commit of {pr}; r reloads it"
        )),
        None => Err(format!(
            "the state of {pr} is not loaded, so the commit to review is not known; r reloads it"
        )),
    }
}

/// `a`: asks to approve the pull request, on the head commit shown.
pub fn ask_approve(ed: &mut Editor) {
    let (pr, title) = match super::screen::target_pr(ed) {
        Ok(t) => t,
        Err(e) => {
            ed.error(e);
            return;
        }
    };
    match shown_head(ed, &pr) {
        Ok(head) => ask(ed, pr, &title, WriteAction::Approve { head }, None),
        Err(e) => ed.error(e),
    }
}

/// `c` and `R`: opens a buffer for the message (or the one already being
/// written for this pull request), in insert mode.
pub fn start_message(ed: &mut Editor, kind: DraftKind) {
    let (pr, _) = match super::screen::target_pr(ed) {
        Ok(t) => t,
        Err(e) => {
            ed.error(e);
            return;
        }
    };
    // A change request is a review of the commit on screen now.
    let head = match kind {
        DraftKind::Comment => None,
        DraftKind::RequestChanges => match shown_head(ed, &pr) {
            Ok(h) => Some(h),
            Err(e) => {
                ed.error(e);
                return;
            }
        },
    };
    ed.forge.drafts.retain(|d| ed.buffers.contains_key(&d.doc));
    let existing = ed
        .forge
        .drafts
        .iter()
        .find(|d| d.pr == pr && d.kind == kind && d.head == head)
        .map(|d| d.doc);
    let doc = match existing {
        Some(doc) => doc,
        None => {
            let doc = ed.add_document(maestro_core::Document::default());
            ed.forge.drafts.push(MessageDraft {
                doc,
                pr: pr.clone(),
                kind,
                head,
            });
            doc
        }
    };
    super::screen::close(ed);
    ed.switch_to(doc);
    if ed.mode != Mode::Insert {
        ed.run_command("insert_mode");
    }
    ed.info(format!(
        "{} for {pr}: write it here, then :pr-send shows exactly what will be sent; \
         :bd! drops it",
        kind.label()
    ));
}

/// `:pr-send`: the confirmation for the message in the current buffer.
pub fn send_draft(ed: &mut Editor) -> Result<(), String> {
    ed.forge.drafts.retain(|d| ed.buffers.contains_key(&d.doc));
    let doc = ed.view().doc;
    let Some(draft) = ed.forge.drafts.iter().find(|d| d.doc == doc).cloned() else {
        return Err(if ed.forge.drafts.is_empty() {
            "no message to send: c (comment) or R (request changes) in the GitHub inbox \
             (space i) starts one"
                .into()
        } else {
            "this buffer is not a GitHub message; switch to the message buffer (space b) first"
                .into()
        });
    };
    let text = ed.doc().text().to_string();
    let body = text.trim_end().to_string();
    if body.trim().is_empty() {
        return Err(format!(
            "the {} is empty; write it first",
            draft.kind.label()
        ));
    }
    let action = match (draft.kind, draft.head.clone()) {
        (DraftKind::Comment, _) => WriteAction::Comment { body },
        (DraftKind::RequestChanges, Some(head)) => WriteAction::RequestChanges { body, head },
        (DraftKind::RequestChanges, None) => {
            return Err(
                "this change request has no commit to review; start it again with R".into(),
            );
        }
    };
    let title = known_title(ed, &draft.pr);
    if ed.mode == Mode::Insert {
        ed.run_command("normal_mode");
    }
    let version = ed.doc().version();
    ask(ed, draft.pr, &title, action, Some((draft.doc, version)));
    Ok(())
}

/// The list item for `pr`: the selected row when it is that pull request
/// (what the user is looking at), else its first row.
fn known_item<'a>(ed: &'a Editor, pr: &PrRef) -> Option<&'a maestro_forge::Item> {
    if let Some((_, it)) = super::screen::selected_item(ed)
        && it.reference == *pr
    {
        return Some(it);
    }
    ed.forge
        .inbox
        .iter()
        .flat_map(|i| &i.sections)
        .filter_map(|s| s.items.as_ref().ok())
        .flatten()
        .find(|i| i.reference == *pr)
}

/// The detail of `pr`, when the detail view shows it loaded.
fn known_detail<'a>(ed: &'a Editor, pr: &PrRef) -> Option<&'a maestro_forge::PrDetail> {
    ed.forge
        .screen
        .detail
        .as_ref()
        .filter(|v| v.pr == *pr)
        .and_then(|v| v.detail())
}

/// The title of `pr` from the detail view or the list, if loaded.
fn known_title(ed: &Editor, pr: &PrRef) -> String {
    known_detail(ed, pr)
        .map(|d| d.title.clone())
        .or_else(|| known_item(ed, pr).map(|i| i.title.clone()))
        .unwrap_or_default()
}

/// The status of `pr` and its base branch, from the detail view when it
/// is loaded, else from the list.
fn known_status(ed: &Editor, pr: &PrRef) -> Option<(PrStatus, Option<String>)> {
    if let Some(d) = known_detail(ed, pr) {
        return Some((d.status.clone(), Some(d.base_ref.clone())));
    }
    known_item(ed, pr)
        .and_then(|i| i.pr_status().cloned())
        .map(|s| (s, None))
}

/// `m`: the choice of merge method.
pub fn ask_merge(ed: &mut Editor) {
    let (pr, title) = match super::screen::target_pr(ed) {
        Ok(t) => t,
        Err(e) => {
            ed.error(e);
            return;
        }
    };
    if let Some(w) = &ed.forge.writing {
        ed.error(format!("still sending: {w}; wait for it to finish"));
        return;
    }
    let Some((status, base)) = known_status(ed, &pr) else {
        ed.error(format!(
            "the state of {pr} is not loaded, so its checks cannot be shown before merging; \
             r reloads it"
        ));
        return;
    };
    if status.head_oid.is_empty() {
        ed.error(format!(
            "GitHub did not give the head commit of {pr}; r reloads it"
        ));
        return;
    }
    ed.forge.screen.dialog = Some(Dialog::Merge {
        pr,
        title,
        status,
        base,
        selected: 0,
    });
}

/// The merge dialog's warnings, from the state shown on screen.
pub fn merge_warnings(status: &PrStatus) -> Vec<String> {
    let mut out = Vec::new();
    if status.draft {
        out.push(
            "WARNING: this pull request is a draft. GitHub refuses to merge a draft until it \
             is marked ready for review."
                .into(),
        );
    }
    let sum = status.summary();
    let names = |st: CheckState| {
        let v: Vec<&str> = status
            .checks
            .iter()
            .filter(|c| c.state == st)
            .map(|c| c.name.as_str())
            .collect();
        let shown = v.iter().take(5).copied().collect::<Vec<_>>().join(", ");
        if v.len() > 5 {
            format!("{shown} and {} more", v.len() - 5)
        } else {
            shown
        }
    };
    if sum.failed > 0 {
        out.push(format!(
            "WARNING: {} of {} checks failed: {}.",
            sum.failed,
            sum.total(),
            names(CheckState::Failed)
        ));
    }
    if sum.pending > 0 {
        out.push(format!(
            "WARNING: {} of {} checks have not finished: {}.",
            sum.pending,
            sum.total(),
            names(CheckState::Pending)
        ));
    }
    if sum.total() == 0 {
        out.push("No checks are reported for the head commit.".into());
    }
    match status.mergeable {
        Mergeable::Conflicting => {
            out.push("WARNING: GitHub reports merge conflicts with the base branch.".into())
        }
        Mergeable::Unknown => {
            out.push("GitHub has not said yet whether it merges without conflicts.".into())
        }
        Mergeable::Mergeable => {}
    }
    match status.review_decision {
        ReviewDecision::ChangesRequested => {
            out.push("WARNING: a reviewer requested changes.".into())
        }
        ReviewDecision::ReviewRequired => {
            out.push("WARNING: the reviews the repository requires are not done yet.".into())
        }
        _ => {}
    }
    out
}

fn ask_merge_method(
    ed: &mut Editor,
    pr: PrRef,
    title: &str,
    status: &PrStatus,
    base: Option<String>,
    method: MergeMethod,
) {
    let action = WriteAction::Merge {
        method,
        head: status.head_oid.clone(),
    };
    let into = base.map(|b| format!(" into {b}")).unwrap_or_default();
    let mut lines = vec![format!(
        "Merge {pr}{into} on GitHub by {}: it {}.",
        method.label(),
        method.detail()
    )];
    if !title.is_empty() {
        lines.push(format!("\"{}\"", visible(title)));
    }
    lines.extend(merge_warnings(status));
    lines.extend(pinned_line(&action));
    lines.push(
        "Maestro does not ask GitHub to delete the branch; the repository's own setting \
         may delete it after the merge."
            .into(),
    );
    lines.push(
        "If the base branch uses a merge queue, GitHub queues it instead of merging now \
         (the queue decides how it is merged), or turns on auto-merge until checks pass. \
         The status line says which happened."
            .into(),
    );
    lines.extend(command_lines(&pr, &action));
    ed.forge.screen.dialog = Some(Dialog::Confirm {
        title: format!("merge {pr} ({})", method.label()),
        lines,
        keys: confirm_keys(),
        pr,
        action,
        draft: None,
    });
}

/// Keys while a dialog is up.
pub fn dialog_key(ed: &mut Editor, key: KeyEvent) {
    let Some(dialog) = ed.forge.screen.dialog.take() else {
        return;
    };
    let ch = key.typed_char();
    let cancel =
        key.code == KeyCode::Esc || (key.mods.ctrl && ch.is_none()) || key == KeyEvent::ctrl('c');
    match dialog {
        Dialog::Confirm {
            title,
            lines,
            keys,
            pr,
            action,
            draft,
        } => {
            if cancel || ch == Some('n') {
                ed.info(format!("{title}: canceled, nothing was sent"));
                return;
            }
            if ch == Some('y') {
                if crate::agents::panel::dialog_fits(ed.area, &lines, &keys) {
                    send(ed, pr, action, draft);
                } else {
                    ed.error(
                        "the window is too small to show this whole dialog; make it larger, \
                         or press n",
                    );
                    ed.forge.screen.dialog = Some(Dialog::Confirm {
                        title,
                        lines,
                        keys,
                        pr,
                        action,
                        draft,
                    });
                }
                return;
            }
            ed.forge.screen.dialog = Some(Dialog::Confirm {
                title,
                lines,
                keys,
                pr,
                action,
                draft,
            });
        }
        Dialog::Merge {
            pr,
            title,
            status,
            base,
            mut selected,
        } => {
            if cancel || ch == Some('q') {
                ed.info(format!("merge {pr}: canceled, nothing was sent"));
                return;
            }
            let n = MergeMethod::ALL.len();
            let chosen = match (key.code, ch) {
                (KeyCode::Enter, _) => Some(MergeMethod::ALL[selected]),
                (_, Some('m')) => Some(MergeMethod::Merge),
                (_, Some('s')) => Some(MergeMethod::Squash),
                (_, Some('r')) => Some(MergeMethod::Rebase),
                (KeyCode::Down, _) | (_, Some('j')) => {
                    selected = (selected + 1) % n;
                    None
                }
                (KeyCode::Up, _) | (_, Some('k')) => {
                    selected = (selected + n - 1) % n;
                    None
                }
                _ => None,
            };
            match chosen {
                Some(m) => ask_merge_method(ed, pr, &title, &status, base, m),
                None => {
                    ed.forge.screen.dialog = Some(Dialog::Merge {
                        pr,
                        title,
                        status,
                        base,
                        selected,
                    })
                }
            }
        }
    }
}

/// Runs a confirmed write action in the background.
fn send(ed: &mut Editor, pr: PrRef, action: WriteAction, draft: Option<Draft>) {
    let gh = super::gh(ed);
    let what = format!("{} {pr}", action.verb());
    ed.forge.writing = Some(what.clone());
    ed.info(format!("sending: {what}…"));
    let spawned = ed.spawn_background("github-write", move |tx| {
        let result = action.run(&gh, &pr);
        tx.send(move |ed| sent(ed, pr, action, draft, result));
    });
    if let Err(e) = spawned {
        ed.forge.writing = None;
        ed.error(format!("could not start sending: {e}"));
    }
}

fn sent(
    ed: &mut Editor,
    pr: PrRef,
    action: WriteAction,
    draft: Option<Draft>,
    result: maestro_forge::Result<maestro_forge::Outcome>,
) {
    ed.forge.writing = None;
    match result {
        Ok(outcome) => {
            let mut note = "";
            if let Some((doc, version)) = draft
                && let Some(b) = ed.buffers.get(&doc)
            {
                if b.doc.version() == version {
                    // The text is on GitHub now; the buffer was only a draft.
                    let _ = ed.close_buffer(doc, true);
                } else {
                    // Typed into while it was being sent: that text was not
                    // sent, so the buffer stays (no longer a draft).
                    note = "; the message buffer changed after it was sent, so it is kept";
                }
                ed.forge.drafts.retain(|d| d.doc != doc);
            }
            match outcome {
                maestro_forge::Outcome::MergeState(Ok(s)) if s != "MERGED" => ed.info(format!(
                    "gh accepted the merge of {pr}, and GitHub reports it {}: a merge queue \
                     or auto-merge has it, not merged yet",
                    s.to_lowercase()
                )),
                maestro_forge::Outcome::MergeState(Err(e)) => ed.error(format!(
                    "gh accepted the merge of {pr}, but its state could not be read \
                     afterward: {e}"
                )),
                _ => ed.info(format!("{} {pr}{note}", action.done())),
            }
            if ed.forge.screen.detail.as_ref().is_some_and(|d| d.pr == pr) {
                super::detail::load(ed, pr);
            }
            super::refresh(ed);
        }
        Err(e) => {
            let kept = if draft.is_some() {
                "; the message buffer is kept"
            } else {
                ""
            };
            ed.error(format!("could not {} {pr}: {e}{kept}", action.verb()));
        }
    }
}

/// `o`: opens the selected pull request or issue in the browser.
pub fn browse(ed: &mut Editor) {
    let target = match &ed.forge.screen.detail {
        Some(d) => Some((d.pr.clone(), true)),
        None => super::screen::selected_item(ed).map(|(_, it)| (it.reference.clone(), it.is_pr())),
    };
    let Some((pr, is_pr)) = target else {
        ed.error("nothing selected to open");
        return;
    };
    let gh = super::gh(ed);
    ed.info(format!("opening {pr} in the browser…"));
    let spawned = ed.spawn_background("github-browse", move |tx| {
        let r = gh.run(&maestro_forge::browse_command(&pr, is_pr));
        tx.send(move |ed| match r {
            Ok(_) => ed.info(format!("opened {pr} in the browser")),
            Err(e) => ed.error(format!("could not open {pr} in the browser: {e}")),
        });
    });
    if let Err(e) = spawned {
        ed.error(format!("could not start gh: {e}"));
    }
}

/// The local branch a pull request is checked out on.
pub fn pr_branch(pr: &PrRef) -> String {
    format!("pr-{}", pr.number)
}

/// Where the worktree for `pr` goes: next to the main worktree, named
/// after it.
pub fn worktree_path(main: &Path, pr: &PrRef) -> PathBuf {
    let name = main
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    main.with_file_name(format!("{name}-pr-{}", pr.number))
}

/// `w`: checks the pull request out into a new worktree next to the
/// repository and opens a file there. Only for pull requests of the
/// working directory's repository.
pub fn checkout_worktree(ed: &mut Editor) {
    let (pr, _) = match super::screen::target_pr(ed) {
        Ok(t) => t,
        Err(e) => {
            ed.error(e);
            return;
        }
    };
    let current = ed
        .forge
        .inbox
        .as_ref()
        .and_then(|i| i.current_repo())
        .map(String::from);
    match current {
        None => {
            ed.error(
                "w checks out pull requests of the working directory's repository, and it \
                 is not a GitHub repository (or the inbox has not loaded)",
            );
            return;
        }
        Some(cur) if !cur.eq_ignore_ascii_case(&pr.repo) => {
            ed.error(format!(
                "{pr} belongs to {}, not to {cur} (the working directory's repository); w \
                 only checks out pull requests of the current repository",
                pr.repo
            ));
            return;
        }
        Some(_) => {}
    }
    // Files to open, in order: the pull request's own changes first.
    let mut wanted: Vec<String> = ed
        .forge
        .screen
        .detail
        .as_ref()
        .filter(|d| d.pr == pr)
        .and_then(|d| d.detail())
        .map(|d| {
            d.files
                .iter()
                .filter(|f| f.change != "DELETED")
                .map(|f| f.path.clone())
                .collect()
        })
        .unwrap_or_default();
    wanted.extend(["README.md", "README", "Cargo.toml"].map(String::from));
    let gh = super::gh(ed);
    let git = ed.git.git().clone();
    let cwd = ed.cwd.clone();
    ed.info(format!("checking out {pr} into a new worktree…"));
    let spawned = ed.spawn_background("github-worktree", move |tx| {
        let r = worktree_job(&gh, &git, &cwd, &pr, &wanted);
        tx.send(move |ed| worktree_done(ed, pr, r));
    });
    if let Err(e) = spawned {
        ed.error(format!("could not start the checkout: {e}"));
    }
}

/// The worktree and the file to open in it, or what went wrong.
type WorktreeResult = Result<(PathBuf, Option<PathBuf>, bool), String>;

fn worktree_job(
    gh: &maestro_forge::Gh,
    git: &maestro_git::Git,
    cwd: &Path,
    pr: &PrRef,
    wanted: &[String],
) -> WorktreeResult {
    let repo = git
        .discover(cwd)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            let shown = crate::navigation::home_path(cwd);
            format!("{shown} is not in a Git repository")
        })?;
    let trees = repo.worktrees().map_err(|e| e.to_string())?;
    let main = trees
        .first()
        .map(|w| w.path.clone())
        .unwrap_or_else(|| repo.root().to_path_buf());
    // The repository on disk must be the pull request's: the working
    // directory may have changed since the inbox loaded.
    match maestro_forge::current_repo(gh, &main).map_err(|e| e.to_string())? {
        Ok(name) if name.eq_ignore_ascii_case(&pr.repo) => {}
        Ok(name) => {
            return Err(format!(
                "{} is a clone of {name}, not of {}; nothing was checked out",
                crate::navigation::display_path(&main, cwd),
                pr.repo
            ));
        }
        Err(why) => {
            return Err(format!(
                "{} is not a GitHub repository ({why}); nothing was checked out",
                crate::navigation::display_path(&main, cwd)
            ));
        }
    }
    let path = worktree_path(&main, pr);
    let branch = pr_branch(pr);
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let existing = trees.iter().find(|w| canon(&w.path) == canon(&path));
    let reused = match existing {
        Some(w) if w.branch.as_deref() == Some(branch.as_str()) => true,
        Some(_) => {
            return Err(format!(
                "{} is already a worktree on another branch; not touched",
                crate::navigation::display_path(&path, cwd)
            ));
        }
        None if path.exists() => {
            return Err(format!(
                "{} already exists and is not a worktree of this repository; not touched",
                crate::navigation::display_path(&path, cwd)
            ));
        }
        None => {
            if repo.resolve(&format!("refs/heads/{branch}")).is_ok() {
                return Err(format!(
                    "a branch named {branch} already exists; delete or rename it first \
                     (nothing was checked out)"
                ));
            }
            let cmd = maestro_forge::checkout_command(pr, &branch, &path, &main);
            // gh adds a worktree; agents may be adding theirs at once.
            repo.with_worktree_lock(|| gh.run(&cmd))
                .map_err(|e| e.to_string())?;
            false
        }
    };
    let file = wanted
        .iter()
        .map(|w| path.join(w))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file()));
    Ok((path, file, reused))
}

fn worktree_done(ed: &mut Editor, pr: PrRef, r: WorktreeResult) {
    match r {
        Err(e) => ed.error(format!("could not check out {pr}: {e}")),
        Ok((path, file, reused)) => {
            let how = if reused {
                "already checked out"
            } else {
                "checked out"
            };
            let msg = format!(
                "{pr} {how} on branch {} in {}",
                pr_branch(&pr),
                ed.show_path(&path)
            );
            match file {
                Some(f) => {
                    super::screen::close(ed);
                    match ed.open(&f) {
                        Ok(_) => ed.info(msg),
                        Err(e) => {
                            let shown = ed.show_path(&f);
                            ed.error(format!("{msg}; could not open {shown}: {e}"))
                        }
                    }
                }
                None => ed.info(format!("{msg}; no file to open there")),
            }
        }
    }
}
