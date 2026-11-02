//! Draws the GitHub inbox over the editor area: the sections of pull
//! requests and issues, or one pull request's detail with its diff in the
//! agent review screen's layout and colors, and the write dialogs. Reads
//! the editor; never changes it.

use maestro_config::{Style, Theme};
use maestro_editor::Editor;
use maestro_editor::Rect;
use maestro_editor::agents::panel;
use maestro_editor::forge::actions::Dialog;
use maestro_editor::forge::api::{self, ChecksSummary, Item, MergeMethod, Overall, PrStatus};
use maestro_editor::forge::detail::{self, DetailRow, DetailView};
use maestro_editor::forge::screen::{self, ListRow};

use crate::agents::{draw_dialog, draw_row};
use crate::render::{CursorShape, draw_box};
use crate::surface::Surface;

/// The color for an overall checks state. It is always drawn next to its
/// symbol and word.
pub fn overall_style(theme: &Theme, o: Overall) -> Style {
    let a = &theme.agent;
    match o {
        Overall::Passed => a.completed,
        Overall::Failed => a.failed,
        Overall::Pending => a.running,
        Overall::None => theme.ui.text_muted,
    }
}

fn check_style(theme: &Theme, s: api::CheckState) -> Style {
    use api::CheckState::*;
    match s {
        Passed => overall_style(theme, Overall::Passed),
        Failed => overall_style(theme, Overall::Failed),
        Pending => overall_style(theme, Overall::Pending),
        Skipped => theme.ui.text_muted,
    }
}

/// `✓ passed 6`, `✗ failed 1/9`... for a list row.
pub fn checks_text(sum: &ChecksSummary) -> String {
    format!("{} {}", sum.symbol(), sum.label())
}

/// The checks column of a list row and its style.
fn checks_cell(ed: &Editor, it: &Item) -> (String, Style) {
    let ui = &ed.theme.ui;
    if !it.is_pr() {
        return ("issue".into(), ui.text_muted);
    }
    match &it.status {
        Some(Ok(st)) => {
            let sum = st.summary();
            (checks_text(&sum), overall_style(&ed.theme, sum.overall()))
        }
        Some(Err(_)) => ("? status not loaded".into(), ui.message_warning),
        None => ("? loading".into(), ui.text_muted),
    }
}

pub(crate) fn draw(ed: &Editor, s: &mut Surface) -> CursorShape {
    let area = ed.area;
    let ui = &ed.theme.ui;
    if area.width < 30 || area.height < 6 {
        s.fill(area, ui.text);
        s.print(
            area.x,
            area.y,
            "GitHub inbox: the window is too small",
            ui.message_warning,
            area.right(),
        );
        return CursorShape::Hidden;
    }
    s.fill(area, ui.text);
    match &ed.forge.screen.detail {
        Some(v) => draw_detail(ed, s, v),
        None => draw_list(ed, s),
    }
    match &ed.forge.screen.dialog {
        Some(Dialog::Confirm {
            title, lines, keys, ..
        }) => draw_dialog(ed, s, area, title, lines, keys),
        Some(Dialog::Merge {
            pr,
            status,
            selected,
            ..
        }) => draw_merge_choice(ed, s, area, &pr.to_string(), status, *selected),
        None => {}
    }
    CursorShape::Hidden
}

fn header(ed: &Editor, s: &mut Surface, h: Rect, title: &str, badge: &str) {
    let ui = &ed.theme.ui;
    s.fill(h, ui.statusline);
    let mut x = s.print(h.x, h.y, title, ui.statusline.bold(), h.right());
    let now = maestro_editor::forge::now_unix();
    let state = match (&ed.forge.writing, ed.forge.loading, ed.forge.refreshed) {
        (Some(w), _, _) => format!("sending: {w}…"),
        (None, true, Some(_)) => "refreshing…".into(),
        (None, true, None) => "loading…".into(),
        (None, false, Some((at, _))) => format!(
            "refreshed {} UTC ({} ago)",
            api::utc_clock(at),
            api::age_label(at, now)
        ),
        (None, false, None) => "not loaded".into(),
    };
    x = s.print(x + 1, h.y, &state, ui.statusline_muted, h.right());
    let bw = badge.chars().count() as u16;
    s.print(
        h.right().saturating_sub(bw).max(x + 1),
        h.y,
        badge,
        ui.mode_normal,
        h.right(),
    );
}

fn hints(ed: &Editor, s: &mut Surface, area: Rect) {
    let ui = &ed.theme.ui;
    s.fill(area, ui.text);
    let mut x = area.x + 1;
    for (k, d) in screen::hints(ed) {
        let need = (k.chars().count() + d.chars().count() + 3) as u16;
        if x + need > area.right() {
            break;
        }
        x = s.print(x, area.y, k, ui.text.bold(), area.right());
        x = s.print(x + 1, area.y, d, ui.text_muted, area.right());
        x += 2;
    }
}

fn draw_list(ed: &Editor, s: &mut Surface) {
    let area = ed.area;
    let ui = &ed.theme.ui;
    header(
        ed,
        s,
        Rect::new(area.x, area.y, area.width, 1),
        " GitHub inbox ",
        " KEYS: INBOX ",
    );
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 2);
    let rows = screen::list_rows(ed);
    let cursor = screen::cursor_row(ed, &rows);
    let height = body.height as usize;
    // The stored scroll, moved just enough to show the cursor.
    let mut top = ed.forge.screen.top.min(rows.len().saturating_sub(1));
    if let Some(c) = cursor {
        if c < top {
            top = c;
        } else if c >= top + height {
            top = c + 1 - height;
        }
    }
    let now = maestro_editor::forge::now_unix();
    for (i, row) in rows.iter().enumerate().skip(top).take(height) {
        let y = body.y + (i - top) as u16;
        let line = Rect::new(body.x, y, body.width, 1);
        match row {
            ListRow::Header(_, text) => {
                s.print(line.x + 1, y, text, ui.text.bold(), line.right());
            }
            ListRow::Note(text) => {
                s.print(line.x + 3, y, text, ui.text_muted, line.right());
            }
            ListRow::Error(text) => {
                s.print(line.x + 3, y, text, ui.message_error, line.right());
            }
            ListRow::Item(..) => {
                if let Some((_, it)) = screen::row_item(ed, row) {
                    draw_item(ed, s, line, it, Some(i) == cursor, now);
                }
            }
        }
    }
    hints(ed, s, Rect::new(area.x, area.bottom() - 1, area.width, 1));
}

fn draw_item(ed: &Editor, s: &mut Surface, line: Rect, it: &Item, selected: bool, now: i64) {
    let ui = &ed.theme.ui;
    let bg = if selected { ui.popup } else { ui.text };
    s.fill(line, bg);
    let y = line.y;
    if selected {
        s.put(
            line.x,
            y,
            "▌",
            ui.border_active
                .patch(Style::bg(bg.bg.unwrap_or(ed.theme.background))),
            line.x + 1,
        );
    }
    // Right block, from the right edge: age, checks, author.
    let age = it
        .created_at
        .map(|t| api::age_label(t, now))
        .unwrap_or_default();
    let (checks, cstyle) = checks_cell(ed, it);
    let show_author = line.width >= 80;
    let age_w = 5u16;
    let checks_w = 23u16;
    let author_w = if show_author { 20u16 } else { 0 };
    let right_w = age_w + checks_w + author_w;
    let right_x = line.right().saturating_sub(right_w);
    let mut x = s.print(
        line.x + 2,
        y,
        &it.reference.to_string(),
        bg.bold(),
        right_x.saturating_sub(1),
    );
    if it.draft {
        x = s.print(
            x + 1,
            y,
            "draft",
            bg.patch(ui.text_muted),
            right_x.saturating_sub(1),
        );
    }
    s.print(x + 2, y, &it.title, bg, right_x.saturating_sub(1));
    let mut rx = right_x;
    if show_author {
        let author = if it.author.is_empty() {
            "(no author)"
        } else {
            &it.author
        };
        s.print(rx, y, author, bg.patch(ui.text_muted), rx + author_w - 1);
        rx += author_w;
    }
    s.print(rx, y, &checks, bg.patch(cstyle), rx + checks_w - 1);
    rx += checks_w;
    let aw = age.chars().count() as u16;
    s.print(
        (rx + age_w).saturating_sub(aw + 1),
        y,
        &age,
        bg.patch(ui.text_muted),
        line.right(),
    );
}

fn draw_detail(ed: &Editor, s: &mut Surface, v: &DetailView) {
    let ui = &ed.theme.ui;
    let l = panel::layout(ed.area);
    header(
        ed,
        s,
        l.header,
        &format!(" {} ", v.pr),
        " KEYS: PULL REQUEST ",
    );
    for y in l.divider.y..l.divider.bottom() {
        s.put(l.divider.x, y, "│", ui.border, l.divider.right());
    }
    // Pages: the overview, then each file of the diff.
    let list = l.list;
    let mut pages: Vec<(String, String)> = vec![("overview".into(), String::new())];
    for f in v.files() {
        let (a, r) = f.counts();
        let letter = match f.change {
            maestro_editor::agents::FileChange::Added => "A",
            maestro_editor::agents::FileChange::Deleted => "D",
            maestro_editor::agents::FileChange::Renamed { .. } => "R",
            maestro_editor::agents::FileChange::Copied { .. } => "C",
            maestro_editor::agents::FileChange::Modified => "M",
        };
        let path = f
            .path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        pages.push((format!("{letter} {path}"), format!(" +{a} -{r}")));
    }
    let visible = list.height as usize;
    let first = if v.page >= visible {
        v.page + 1 - visible
    } else {
        0
    };
    for (i, (name, counts)) in pages.iter().enumerate().skip(first).take(visible) {
        let y = list.y + (i - first) as u16;
        let selected = i == v.page;
        let bg = if selected { ui.popup } else { ui.text };
        s.fill(Rect::new(list.x, y, list.width, 1), bg);
        if selected {
            s.put(
                list.x,
                y,
                "▌",
                ui.border_active
                    .patch(Style::bg(bg.bg.unwrap_or(ed.theme.background))),
                list.x + 1,
            );
        }
        let cw = counts.chars().count() as u16;
        let name_r = list.right().saturating_sub(cw);
        s.print(list.x + 2, y, name, bg, name_r);
        s.print(name_r, y, counts, bg.patch(ui.text_muted), list.right());
    }
    draw_summary(ed, s, l.summary, v);
    // Content.
    let c = l.content;
    let rows = detail::rows(ed);
    for (i, row) in rows.iter().enumerate().skip(v.top).take(c.height as usize) {
        let y = c.y + (i - v.top) as u16;
        let r = c.right();
        match row {
            DetailRow::Heading(t) => {
                s.print(c.x + 2, y, t, ui.text.bold(), r);
            }
            DetailRow::Text(t) => {
                s.print(c.x + 2, y, t, ui.text, r);
            }
            DetailRow::Muted(t) => {
                s.print(c.x + 2, y, t, ui.text_muted, r);
            }
            DetailRow::Error(t) => {
                s.print(c.x + 2, y, t, ui.message_error, r);
            }
            DetailRow::Check(ch) => {
                let st = check_style(&ed.theme, ch.state);
                let x = s.print(
                    c.x + 2,
                    y,
                    &format!("{} {}", ch.state.symbol(), ch.state.word()),
                    ui.text.patch(st),
                    r,
                );
                let name = if ch.workflow.is_empty() {
                    ch.name.clone()
                } else {
                    format!("{} / {}", ch.workflow, ch.name)
                };
                let x = s.print((c.x + 13).max(x + 1), y, &name, ui.text, r);
                // GitHub's own word, when it says more than the state.
                let plain = matches!(
                    (ch.state, ch.detail.as_str()),
                    (api::CheckState::Passed, "success")
                        | (api::CheckState::Failed, "failure")
                        | (api::CheckState::Pending, "pending")
                );
                if !plain && !ch.detail.is_empty() {
                    s.print(x + 2, y, &ch.detail, ui.text_muted, r);
                }
            }
            DetailRow::Diff(d) => {
                draw_row(ed, s, Rect::new(c.x, y, c.width, 1), d, v.left);
            }
        }
        if i == v.cursor {
            s.put(c.x, y, "▶", ui.border_active, c.x + 1);
        }
    }
    hints(ed, s, l.hints);
}

fn draw_summary(ed: &Editor, s: &mut Surface, sm: Rect, v: &DetailView) {
    let ui = &ed.theme.ui;
    let r = sm.right();
    let now = maestro_editor::forge::now_unix();
    let Some(d) = v.detail() else {
        let text = match &v.detail {
            Some(Err(e)) => (format!("could not load {}: {e}", v.pr), ui.message_error),
            _ => (format!("loading {} from GitHub…", v.pr), ui.text_muted),
        };
        s.print(sm.x + 1, sm.y, &text.0, text.1, r);
        return;
    };
    // Row 0: number, state, author, age, branches.
    let mut x = s.print(sm.x + 1, sm.y, &d.title, ui.text.bold(), r);
    let age = d
        .created_at
        .map(|t| format!(", {} ago", api::age_label(t, now)))
        .unwrap_or_default();
    let state = d.state.to_lowercase();
    let draft = if d.status.draft { " draft" } else { "" };
    x = s.print(
        x + 2,
        sm.y,
        &format!("{state}{draft}, by {}{age}", d.author),
        ui.text_muted,
        r,
    );
    let _ = x;
    if sm.height > 1 {
        let y = sm.y + 1;
        let x = s.print(
            sm.x + 1,
            y,
            &format!("{} ← {}", d.base_ref, d.head_ref),
            ui.text_muted,
            r,
        );
        let fork = if d.cross_repo { " (from a fork)" } else { "" };
        s.print(x, y, fork, ui.text_muted, r);
    }
    if sm.height > 2 {
        draw_state_row(ed, s, sm.x + 1, sm.y + 2, r, &d.status);
    }
    if sm.height > 3 {
        let y = sm.y + 3;
        let labels = if d.labels.is_empty() {
            "no labels".to_string()
        } else {
            format!("labels: {}", d.labels.join(", "))
        };
        let page = match v.file() {
            Some(f) => format!(
                "{}  [{} layout, s]",
                maestro_editor::agents::review::change_label(f),
                if v.side_by_side {
                    "side-by-side"
                } else {
                    "unified"
                }
            ),
            None if v.loading => "reloading…".into(),
            None => String::new(),
        };
        let x = s.print(sm.x + 1, y, &labels, ui.text_muted, r);
        s.print(x + 3, y, &page, ui.text_muted, r);
    }
}

/// `review: approved   merge: no conflicts   checks: ✓ passed 9`.
fn draw_state_row(ed: &Editor, s: &mut Surface, x0: u16, y: u16, r: u16, st: &PrStatus) {
    let ui = &ed.theme.ui;
    let mut x = s.print(x0, y, "review: ", ui.text_muted, r);
    let review_style = match st.review_decision {
        api::ReviewDecision::Approved => ed.theme.agent.completed,
        api::ReviewDecision::ChangesRequested => ed.theme.agent.failed,
        _ => ui.text,
    };
    x = s.print(
        x,
        y,
        st.review_decision.label(),
        ui.text.patch(review_style),
        r,
    );
    x = s.print(x + 3, y, "merge: ", ui.text_muted, r);
    let merge_style = match st.mergeable {
        api::Mergeable::Conflicting => ed.theme.agent.failed,
        _ => ui.text,
    };
    x = s.print(x, y, st.mergeable.label(), ui.text.patch(merge_style), r);
    x = s.print(x + 3, y, "checks: ", ui.text_muted, r);
    let sum = st.summary();
    s.print(
        x,
        y,
        &checks_text(&sum),
        ui.text.patch(overall_style(&ed.theme, sum.overall())),
        r,
    );
}

fn draw_merge_choice(
    ed: &Editor,
    s: &mut Surface,
    area: Rect,
    pr: &str,
    status: &PrStatus,
    selected: usize,
) {
    let ui = &ed.theme.ui;
    let (w, _) = panel::dialog_width(area);
    let x = area.x + (area.width - w) / 2;
    let h = (MergeMethod::ALL.len() as u16 + 4).min(area.height);
    let y = area.y + (area.height.saturating_sub(h)) / 3;
    let b = Rect::new(x, y, w, h);
    draw_box(
        s,
        b,
        &format!("merge {pr}: how?"),
        ui.popup,
        ui.popup_border,
        ui.popup_title,
    );
    for (i, m) in MergeMethod::ALL.iter().enumerate() {
        let ry = y + 1 + i as u16;
        if ry + 1 >= b.bottom() {
            break;
        }
        let style = if i == selected {
            ui.popup_selected
        } else {
            ui.popup
        };
        s.fill(Rect::new(x + 1, ry, w - 2, 1), style);
        let key = match m {
            MergeMethod::Merge => "m",
            MergeMethod::Squash => "s",
            MergeMethod::Rebase => "r",
        };
        let lx = s.print(x + 2, ry, key, ui.popup_title.patch(style), b.right() - 1);
        let lx = s.print(lx + 1, ry, m.label(), style, b.right() - 1);
        let dstyle = if i == selected { style } else { ui.popup_muted };
        s.print(lx + 2, ry, m.detail(), dstyle, b.right() - 1);
    }
    let sum = status.summary();
    let ky = b.bottom().saturating_sub(2);
    let kx = s.print(x + 2, ky, "checks: ", ui.popup_muted, b.right() - 1);
    s.print(
        kx,
        ky,
        &checks_text(&sum),
        ui.popup.patch(overall_style(&ed.theme, sum.overall())),
        b.right() - 1,
    );
}

#[cfg(test)]
#[path = "inbox_tests.rs"]
mod tests;
