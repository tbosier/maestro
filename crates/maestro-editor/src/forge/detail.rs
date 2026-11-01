//! One pull request in detail: its description as plain text, labels,
//! reviews, checks by name, and the changed files with the diff drawn
//! like the agent review screen (the same rows, intraline marks and
//! colors). Pages: the overview first, then one per changed file.

use maestro_forge::{Check, ForgeError, PrDetail, PrRef};
use maestro_git::DiffFile;

use crate::agents::review::Row;
use crate::editor::Editor;
use crate::key::{KeyCode, KeyEvent};

#[derive(Debug, Clone)]
pub struct DetailView {
    pub pr: PrRef,
    /// `None` while loading.
    pub detail: Option<Result<PrDetail, ForgeError>>,
    pub diff: Option<Result<Vec<DiffFile>, ForgeError>>,
    pub loading: bool,
    /// The load whose answer is awaited (see `Forge::detail_seq`).
    seq: u64,
    /// 0 is the overview; `n` is diff file `n - 1`.
    pub page: usize,
    /// Row under the cursor and first row shown.
    pub cursor: usize,
    pub top: usize,
    /// Columns of each diff line scrolled off to the left (`h`, `l`).
    pub left: usize,
    pub side_by_side: bool,
}

impl DetailView {
    pub fn detail(&self) -> Option<&PrDetail> {
        self.detail.as_ref()?.as_ref().ok()
    }

    pub fn files(&self) -> &[DiffFile] {
        match &self.diff {
            Some(Ok(f)) => f,
            _ => &[],
        }
    }

    /// The diff file the page shows, if it is not the overview.
    pub fn file(&self) -> Option<&DiffFile> {
        self.page.checked_sub(1).and_then(|i| self.files().get(i))
    }

    pub fn pages(&self) -> usize {
        1 + self.files().len()
    }
}

/// One line of the detail view's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailRow {
    Heading(String),
    Text(String),
    Muted(String),
    Error(String),
    Check(Check),
    /// A line of the diff, drawn as the review screen draws it.
    Diff(Row),
}

/// Opens the detail view of `pr` and loads it.
pub fn open(ed: &mut Editor, pr: PrRef) {
    ed.forge.screen.detail = Some(DetailView {
        pr: pr.clone(),
        detail: None,
        diff: None,
        loading: false,
        seq: 0,
        page: 0,
        cursor: 0,
        top: 0,
        left: 0,
        side_by_side: false,
    });
    load(ed, pr);
}

/// Loads (or reloads) the detail and diff of `pr` in the background.
pub fn load(ed: &mut Editor, pr: PrRef) {
    let gh = super::gh(ed);
    let Some(v) = ed.forge.screen.detail.as_mut().filter(|v| v.pr == pr) else {
        return;
    };
    ed.forge.detail_seq += 1;
    let seq = ed.forge.detail_seq;
    v.seq = seq;
    v.loading = true;
    let spawned = ed.spawn_background("github-pr", move |tx| {
        // Checked to be of one head commit, so the diff shown is of the
        // commit a review or merge from this view is pinned to.
        let (detail, diff) = maestro_forge::pr_detail_and_diff(&gh, &pr);
        tx.send(move |ed| loaded(ed, pr, seq, detail, diff));
    });
    if let Err(e) = spawned
        && let Some(v) = ed.forge.screen.detail.as_mut()
    {
        v.loading = false;
        ed.error(format!("could not start loading the pull request: {e}"));
    }
}

fn loaded(
    ed: &mut Editor,
    pr: PrRef,
    seq: u64,
    detail: Result<PrDetail, ForgeError>,
    diff: Result<Vec<DiffFile>, ForgeError>,
) {
    let Some(v) = ed.forge.screen.detail.as_mut() else {
        return;
    };
    if v.pr != pr || v.seq != seq {
        return;
    }
    // A reload keeps the page when the same file is still there.
    let path = v.file().and_then(|f| f.path().cloned());
    v.loading = false;
    v.detail = Some(detail);
    v.diff = Some(diff);
    v.page = match path {
        Some(p) => v
            .files()
            .iter()
            .position(|f| f.path() == Some(&p))
            .map_or(0, |i| i + 1),
        None => 0,
    };
    let n = rows(ed).len();
    if let Some(v) = ed.forge.screen.detail.as_mut() {
        v.cursor = v.cursor.min(n.saturating_sub(1));
        v.top = v.top.min(v.cursor);
    }
}

/// The content area the detail view draws into.
pub fn content_area(ed: &Editor) -> crate::layout::Rect {
    crate::agents::panel::layout(ed.area).content
}

/// The rows of the current page.
pub fn rows(ed: &Editor) -> Vec<DetailRow> {
    let Some(v) = &ed.forge.screen.detail else {
        return Vec::new();
    };
    if let Some(f) = v.file() {
        return crate::agents::review::rows(f, v.side_by_side, &[])
            .into_iter()
            .map(DetailRow::Diff)
            .collect();
    }
    let width = (content_area(ed).width as usize).saturating_sub(4).max(10);
    overview_rows(v, width, super::now_unix())
}

fn wrapped(text: &str, width: usize, indent: &str) -> Vec<String> {
    text.lines()
        .flat_map(|l| {
            if l.is_empty() {
                vec![String::new()]
            } else {
                crate::agents::panel::wrap_words(&l.replace('\t', "    "), width)
                    .into_iter()
                    .map(|w| format!("{indent}{w}"))
                    .collect()
            }
        })
        .collect()
}

/// The overview page: checks, reviews, the description, files. The
/// description comes after the short facts because it is often long.
pub fn overview_rows(v: &DetailView, width: usize, now: i64) -> Vec<DetailRow> {
    let mut out = Vec::new();
    let d = match &v.detail {
        None => {
            out.push(DetailRow::Muted(
                "loading the pull request from GitHub…".into(),
            ));
            return out;
        }
        Some(Err(e)) => {
            out.push(DetailRow::Error(format!("could not load it: {e}")));
            out.push(DetailRow::Muted("r tries again".into()));
            return out;
        }
        Some(Ok(d)) => d,
    };
    let sum = d.status.summary();
    out.push(DetailRow::Heading(format!(
        "Checks: {} {}",
        sum.symbol(),
        sum.label()
    )));
    if d.status.checks.is_empty() {
        out.push(DetailRow::Muted(
            "no checks are reported for the head commit".into(),
        ));
    }
    // Failures first, then what is still running, then the rest.
    let mut checks = d.status.checks.clone();
    checks.sort_by_key(|c| match c.state {
        maestro_forge::CheckState::Failed => 0,
        maestro_forge::CheckState::Pending => 1,
        maestro_forge::CheckState::Passed => 2,
        maestro_forge::CheckState::Skipped => 3,
    });
    out.extend(checks.into_iter().map(DetailRow::Check));
    out.push(DetailRow::Text(String::new()));
    out.push(DetailRow::Heading(format!("Reviews ({})", d.reviews.len())));
    if d.reviews.is_empty() {
        out.push(DetailRow::Muted("no reviews yet".into()));
    }
    for r in &d.reviews {
        let when = r
            .submitted_at
            .map(|t| format!(", {} ago", maestro_forge::age_label(t, now)))
            .unwrap_or_default();
        let state = r.state.to_lowercase().replace('_', " ");
        out.push(DetailRow::Text(format!("{}: {state}{when}", r.author)));
        let body = maestro_forge::plain_text(&r.body);
        out.extend(
            wrapped(&body, width.saturating_sub(4), "    ")
                .into_iter()
                .map(DetailRow::Muted),
        );
    }
    out.push(DetailRow::Text(String::new()));
    out.push(DetailRow::Heading("Description".into()));
    let body = maestro_forge::plain_text(&d.body);
    if body.trim().is_empty() {
        out.push(DetailRow::Muted("no description".into()));
    } else {
        out.extend(wrapped(&body, width, "").into_iter().map(DetailRow::Text));
    }
    out.push(DetailRow::Text(String::new()));
    let (add, del) = d
        .files
        .iter()
        .fold((0, 0), |(a, r), f| (a + f.additions, r + f.deletions));
    out.push(DetailRow::Heading(format!(
        "Files ({}, +{add} -{del}); tab shows each diff",
        d.files.len()
    )));
    for f in &d.files {
        out.push(DetailRow::Text(format!(
            "{} {}  +{} -{}",
            change_letter(&f.change),
            f.path,
            f.additions,
            f.deletions
        )));
    }
    match &v.diff {
        None => out.push(DetailRow::Muted("loading the diff…".into())),
        Some(Err(e)) => out.push(DetailRow::Error(format!(
            "the diff could not be loaded: {e}"
        ))),
        Some(Ok(_)) => {}
    }
    out
}

fn change_letter(change: &str) -> &'static str {
    match change {
        "ADDED" => "A",
        "DELETED" => "D",
        "RENAMED" => "R",
        "COPIED" => "C",
        _ => "M",
    }
}

pub fn handle_key(ed: &mut Editor, key: KeyEvent) {
    let rows = rows(ed);
    let height = content_area(ed).height.max(1) as usize;
    let step = (content_area(ed).width as usize / 2).max(1);
    let Some(v) = ed.forge.screen.detail.as_mut() else {
        return;
    };
    let last = rows.len().saturating_sub(1);
    let pages = v.pages();
    let ctrl = key.mods.ctrl;
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') if !ctrl => {
            ed.forge.screen.detail = None;
            return;
        }
        KeyCode::Char('j') | KeyCode::Down if !ctrl => v.cursor = (v.cursor + 1).min(last),
        KeyCode::Char('k') | KeyCode::Up if !ctrl => v.cursor = v.cursor.saturating_sub(1),
        KeyCode::PageDown => v.cursor = (v.cursor + height / 2).min(last),
        KeyCode::PageUp => v.cursor = v.cursor.saturating_sub(height / 2),
        KeyCode::Char('g') if !ctrl => v.cursor = 0,
        KeyCode::Char('G') if !ctrl => v.cursor = last,
        KeyCode::Tab | KeyCode::Char('}') => {
            v.page = (v.page + 1) % pages;
            v.cursor = 0;
            v.top = 0;
            v.left = 0;
        }
        KeyCode::BackTab | KeyCode::Char('{') => {
            v.page = (v.page + pages - 1) % pages;
            v.cursor = 0;
            v.top = 0;
            v.left = 0;
        }
        KeyCode::Char('l') | KeyCode::Right if !ctrl => v.left += step,
        KeyCode::Char('h') | KeyCode::Left if !ctrl => v.left = v.left.saturating_sub(step),
        KeyCode::Char(']') if !ctrl => {
            if let Some(i) = rows
                .iter()
                .enumerate()
                .skip(v.cursor + 1)
                .find(|(_, r)| matches!(r, DetailRow::Diff(Row::Hunk { .. })))
                .map(|(i, _)| i)
            {
                v.cursor = i;
            }
        }
        KeyCode::Char('[') if !ctrl => {
            if let Some(i) = rows
                .iter()
                .enumerate()
                .take(v.cursor)
                .rev()
                .find(|(_, r)| matches!(r, DetailRow::Diff(Row::Hunk { .. })))
                .map(|(i, _)| i)
            {
                v.cursor = i;
            }
        }
        KeyCode::Char('s') if !ctrl => {
            v.side_by_side = !v.side_by_side;
            v.cursor = 0;
            v.top = 0;
        }
        _ => {}
    }
    if v.cursor < v.top {
        v.top = v.cursor;
    } else if v.cursor >= v.top + height {
        v.top = v.cursor + 1 - height;
    }
}
