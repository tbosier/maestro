//! The inbox screen: sections of pull requests and issues, a detail view
//! of one pull request, and the dialogs for write actions. Keys go to the
//! screen while it is open.

use maestro_forge::{ForgeError, Item, PrRef, SectionKind};

use super::actions::{self, Dialog};
use super::detail::{self, DetailView};
use crate::editor::{Editor, Mode};
use crate::key::{KeyCode, KeyEvent};

/// Where the screen is. Kept while it is hidden.
#[derive(Debug, Clone, Default)]
pub struct Screen {
    /// The selected row: its section and pull request or issue.
    pub selected: Option<(SectionKind, PrRef)>,
    /// First list row shown.
    pub top: usize,
    pub detail: Option<DetailView>,
    pub dialog: Option<Dialog>,
}

/// One line of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListRow {
    /// A section title, with its count when it loaded.
    Header(SectionKind, String),
    /// `section` and `index` into its items.
    Item(usize, usize),
    /// Text in place of items: empty, not applicable, loading.
    Note(String),
    /// A query that failed, in gh's words.
    Error(String),
}

/// What the screen says when there is nothing to list.
pub const NOT_LOADED: &str = "loading the inbox from GitHub…";

/// The list's rows, from the last load.
pub fn list_rows(ed: &Editor) -> Vec<ListRow> {
    let Some(inbox) = &ed.forge.inbox else {
        return vec![ListRow::Note(if ed.forge.loading {
            NOT_LOADED.into()
        } else {
            "not loaded yet: r loads it".into()
        })];
    };
    if let Some(f) = &inbox.fatal {
        let mut rows = vec![ListRow::Error(f.to_string())];
        match f {
            ForgeError::NotLoggedIn => rows.push(ListRow::Note(
                "Run `gh auth login` in a terminal and follow its steps, then press r here.".into(),
            )),
            ForgeError::NotInstalled { .. } => rows.push(ListRow::Note(
                "After installing it, run `gh auth login`, then press r here.".into(),
            )),
            _ => rows.push(ListRow::Note("r tries again.".into())),
        }
        return rows;
    }
    let mut rows = Vec::new();
    for (si, s) in inbox.sections.iter().enumerate() {
        let mut title = s.kind.title().to_string();
        if s.kind == SectionKind::ThisRepository
            && let Ok(name) = &s.repo
        {
            title = format!("{title}: {name}");
        }
        match &s.items {
            Ok(items) if s.limited => {
                title = format!("{title} (first {}; there may be more)", items.len())
            }
            Ok(items) => title = format!("{title} ({})", items.len()),
            Err(_) => {}
        }
        rows.push(ListRow::Header(s.kind, title));
        match (&s.items, &s.repo) {
            (Err(e), _) => rows.push(ListRow::Error(e.to_string())),
            (Ok(_), Err(why)) if s.kind == SectionKind::ThisRepository => {
                rows.push(ListRow::Note(format!(
                    "the working directory is not in a GitHub repository ({})",
                    why.trim()
                )))
            }
            (Ok(items), _) if items.is_empty() => {
                rows.push(ListRow::Note(s.kind.empty_text().into()))
            }
            (Ok(items), _) => rows.extend((0..items.len()).map(|i| ListRow::Item(si, i))),
        }
    }
    rows
}

/// The item a list row points at.
pub fn row_item<'a>(ed: &'a Editor, row: &ListRow) -> Option<(SectionKind, &'a Item)> {
    let ListRow::Item(si, i) = row else {
        return None;
    };
    let s = ed.forge.inbox.as_ref()?.sections.get(*si)?;
    Some((s.kind, s.items.as_ref().ok()?.get(*i)?))
}

/// The row of the selection, or of the first item.
pub fn cursor_row(ed: &Editor, rows: &[ListRow]) -> Option<usize> {
    let items = || rows.iter().position(|r| matches!(r, ListRow::Item(..)));
    let Some((kind, pr)) = &ed.forge.screen.selected else {
        return items();
    };
    rows.iter()
        .position(|r| row_item(ed, r).is_some_and(|(k, i)| k == *kind && i.reference == *pr))
        .or_else(items)
}

/// The selected item.
pub fn selected_item(ed: &Editor) -> Option<(SectionKind, &Item)> {
    let rows = list_rows(ed);
    let at = cursor_row(ed, &rows)?;
    row_item(ed, &rows[at])
}

/// Opens the screen and starts a refresh (the last load stays on screen,
/// with its time, until the new one arrives).
pub fn open(ed: &mut Editor) {
    if ed.mode == Mode::Insert {
        ed.run_command("normal_mode");
    }
    if ed.agents.panel_open() {
        crate::agents::panel::close(ed);
    }
    crate::proposals::review::close(ed);
    ed.forge.open = true;
    super::refresh(ed);
}

pub fn close(ed: &mut Editor) {
    ed.forge.open = false;
    ed.forge.screen.dialog = None;
}

/// Keeps the selection on something that still exists after a reload.
pub fn after_load(ed: &mut Editor) {
    let rows = list_rows(ed);
    let keep = cursor_row(ed, &rows)
        .and_then(|i| row_item(ed, &rows[i]))
        .map(|(k, it)| (k, it.reference.clone()));
    ed.forge.screen.selected = keep;
}

/// Rows the list has room for.
pub fn list_height(ed: &Editor) -> usize {
    ed.area.height.saturating_sub(2).max(1) as usize
}

fn move_selection(ed: &mut Editor, delta: isize) {
    let rows = list_rows(ed);
    let items: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(r, ListRow::Item(..)))
        .map(|(i, _)| i)
        .collect();
    if items.is_empty() {
        return;
    }
    let cur = cursor_row(ed, &rows).unwrap_or(items[0]);
    let at = items.iter().position(|i| *i == cur).unwrap_or(0) as isize;
    let next = (at + delta).clamp(0, items.len() as isize - 1) as usize;
    select_row(ed, &rows, items[next]);
}

fn select_row(ed: &mut Editor, rows: &[ListRow], row: usize) {
    if let Some((k, it)) = row_item(ed, &rows[row]) {
        ed.forge.screen.selected = Some((k, it.reference.clone()));
    }
    // Section headers above the first item stay in view.
    let first_item = rows
        .iter()
        .position(|r| matches!(r, ListRow::Item(..)))
        .unwrap_or(0);
    let height = list_height(ed);
    let s = &mut ed.forge.screen;
    let want = if row == first_item { 0 } else { row };
    if want < s.top {
        s.top = want.saturating_sub(1);
    } else if row >= s.top + height {
        s.top = row + 1 - height;
    }
}

/// The pull request the keys act on: the detail view's, else the selected
/// row's. Issues are named in the error.
pub fn target_pr(ed: &Editor) -> Result<(PrRef, String), String> {
    if let Some(d) = &ed.forge.screen.detail {
        let title = d
            .detail
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .map(|d| d.title.clone())
            .unwrap_or_default();
        return Ok((d.pr.clone(), title));
    }
    match selected_item(ed) {
        Some((_, it)) if it.is_pr() => Ok((it.reference.clone(), it.title.clone())),
        Some((_, it)) => Err(format!(
            "{} is an issue; this works on pull requests (o opens the issue in the browser)",
            it.reference
        )),
        None => Err("no pull request selected".into()),
    }
}

/// Handles a key while the screen is open.
pub fn handle_key(ed: &mut Editor, key: KeyEvent) {
    // A macro or `.` replays keys typed for something else; they must not
    // approve or merge anything.
    if ed.is_replaying() {
        ed.error("the GitHub inbox takes keys typed by hand; replayed keys are ignored");
        return;
    }
    if ed.forge.screen.dialog.is_some() {
        actions::dialog_key(ed, key);
        return;
    }
    let ctrl = key.mods.ctrl;
    if ctrl && key.code == KeyCode::Char('c') {
        close(ed);
        return;
    }
    // Keys that work the same in the list and the detail view.
    match key.code {
        KeyCode::Char('r') if !ctrl => {
            match ed.forge.screen.detail.as_ref().map(|d| d.pr.clone()) {
                Some(pr) => detail::load(ed, pr),
                None => super::refresh(ed),
            }
            return;
        }
        KeyCode::Char('o') if !ctrl => {
            actions::browse(ed);
            return;
        }
        KeyCode::Char('w') if !ctrl => {
            actions::checkout_worktree(ed);
            return;
        }
        KeyCode::Char('a') if !ctrl => {
            actions::ask_approve(ed);
            return;
        }
        KeyCode::Char('c') if !ctrl => {
            actions::start_message(ed, super::DraftKind::Comment);
            return;
        }
        KeyCode::Char('R') if !ctrl => {
            actions::start_message(ed, super::DraftKind::RequestChanges);
            return;
        }
        KeyCode::Char('m') if !ctrl => {
            actions::ask_merge(ed);
            return;
        }
        KeyCode::Char(':') if !ctrl => {
            crate::prompt::open(ed, crate::prompt::PromptKind::Command);
            return;
        }
        _ => {}
    }
    if ed.forge.screen.detail.is_some() {
        detail::handle_key(ed, key);
        return;
    }
    let height = list_height(ed) as isize;
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') if !ctrl => close(ed),
        KeyCode::Char('j') | KeyCode::Down if !ctrl => move_selection(ed, 1),
        KeyCode::Char('k') | KeyCode::Up if !ctrl => move_selection(ed, -1),
        KeyCode::PageDown => move_selection(ed, height / 2),
        KeyCode::PageUp => move_selection(ed, -height / 2),
        KeyCode::Char('g') if !ctrl => move_selection(ed, isize::MIN / 2),
        KeyCode::Char('G') if !ctrl => move_selection(ed, isize::MAX / 2),
        KeyCode::Enter => match selected_item(ed) {
            Some((_, it)) if it.is_pr() => {
                let pr = it.reference.clone();
                detail::open(ed, pr);
            }
            Some((_, it)) => {
                let r = it.reference.to_string();
                ed.info(format!(
                    "{r} is an issue; issues have no detail view here, o opens it in the browser"
                ));
            }
            None => ed.info("nothing selected"),
        },
        _ => {}
    }
}

/// Key hints for the screen's last line.
pub fn hints(ed: &Editor) -> Vec<(&'static str, &'static str)> {
    match &ed.forge.screen.dialog {
        Some(Dialog::Confirm { .. }) => return vec![("y", "send"), ("n/esc", "cancel")],
        Some(Dialog::Merge { .. }) => {
            return vec![
                ("j/k", "choose"),
                ("ret", "next"),
                ("m/s/r", "method"),
                ("esc", "cancel"),
            ];
        }
        None => {}
    }
    if ed.forge.screen.detail.is_some() {
        return vec![
            ("j/k", "scroll"),
            ("tab", "next file"),
            ("]/[", "hunk"),
            ("h/l", "sideways"),
            ("s", "side by side"),
            ("a", "approve"),
            ("c", "comment"),
            ("R", "request changes"),
            ("m", "merge"),
            ("w", "worktree"),
            ("o", "browser"),
            ("r", "reload"),
            ("esc", "back"),
        ];
    }
    vec![
        ("j/k", "move"),
        ("ret", "details"),
        ("o", "browser"),
        ("a", "approve"),
        ("c", "comment"),
        ("R", "request changes"),
        ("m", "merge"),
        ("w", "worktree"),
        ("r", "refresh"),
        ("esc", "close"),
    ]
}
