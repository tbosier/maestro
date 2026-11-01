//! Draws the Git status screen (`space g`) over the editor area, the hunk
//! and blame popup in buffers, and the blame column in the gutter. Reads
//! the editor; never changes it.

use maestro_config::Style;
use maestro_editor::agents::DiffLineKind;
use maestro_editor::agents::review::Row as ReviewRow;
use maestro_editor::git_hunks::{self, Popup};
use maestro_editor::git_screen::{
    self, BranchPicker, DetailRow, Page, Row, Screen, Section, file_label,
};
use maestro_editor::{Editor, Rect, View};

use crate::agents::{draw_dialog, draw_row};
use crate::render::{CursorShape, draw_box};
use crate::surface::Surface;

pub(crate) fn draw_screen(ed: &Editor, s: &mut Surface) -> CursorShape {
    let area = ed.area;
    let ui = &ed.theme.ui;
    let Some(sc) = &ed.git.screen else {
        return CursorShape::Hidden;
    };
    s.fill(area, ui.text);
    if area.width < 20 || area.height < 5 {
        s.print(
            area.x,
            area.y,
            "git: the window is too small",
            ui.message_warning,
            area.right(),
        );
        return CursorShape::Hidden;
    }

    // Header: where, and what is running.
    let h = Rect::new(area.x, area.y, area.width, 1);
    s.fill(h, ui.statusline);
    let root = sc
        .repo
        .as_ref()
        .map(|r| r.root().display().to_string())
        .unwrap_or_else(|| sc.start.display().to_string());
    let title = match sc.pages.last() {
        Some(Page::Log { .. }) => format!(" git log: {root} "),
        Some(Page::Commit { detail, .. }) => {
            format!(
                " git commit {}: {root} ",
                &detail.id[..detail.id.len().min(7)]
            )
        }
        None => format!(" git status: {root} "),
    };
    let x = s.print(h.x, h.y, &title, ui.statusline.bold(), h.right());
    let state = match (&sc.busy, sc.loading) {
        (Some(b), _) => format!(" running: {b} "),
        (None, true) => " loading ".to_string(),
        (None, false) => " KEYS: GIT ".to_string(),
    };
    let sw = state.chars().count() as u16;
    let style = if sc.busy.is_some() || sc.loading {
        ui.mode_insert
    } else {
        ui.mode_normal
    };
    s.print(
        h.right().saturating_sub(sw).max(x + 1),
        h.y,
        &state,
        style,
        h.right(),
    );

    let content = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        git_screen::content_height(area.height) as u16,
    );
    match sc.pages.last() {
        None => draw_status(ed, s, sc, content),
        Some(Page::Log {
            entries,
            cursor,
            top,
        }) => {
            for (i, e) in entries
                .iter()
                .enumerate()
                .skip(*top)
                .take(content.height as usize)
            {
                let y = content.y + (i - top) as u16;
                let row = Rect::new(content.x, y, content.width, 1);
                let base = if i == *cursor {
                    ui.text.patch(ui.cursorline)
                } else {
                    ui.text
                };
                s.fill(row, base);
                let r = row.right();
                let mut x = s.print(row.x + 1, y, &e.graph, base.patch(ed.theme.diff.hunk), r);
                if let Some(c) = &e.commit {
                    x = s.print(x, y, &c.short, base.bold(), r);
                    if !c.refs.is_empty() {
                        x = s.print(
                            x + 1,
                            y,
                            &format!("({})", c.refs),
                            base.patch(ed.theme.diff.git_changed),
                            r,
                        );
                    }
                    x = s.print(x + 1, y, &c.subject, base, r);
                    s.print(
                        x + 2,
                        y,
                        &format!("{}  {}", c.author, c.date),
                        base.patch(ui.text_muted),
                        r,
                    );
                }
            }
        }
        Some(Page::Commit {
            rows, cursor, top, ..
        }) => {
            for (i, row) in rows
                .iter()
                .enumerate()
                .skip(*top)
                .take(content.height as usize)
            {
                let y = content.y + (i - top) as u16;
                let r = Rect::new(content.x, y, content.width, 1);
                draw_detail_row(ed, s, r, row);
                if i == *cursor {
                    s.patch(r, ui.cursorline);
                }
            }
        }
    }

    // Key hints.
    let hl = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    s.fill(hl, ui.text);
    let mut x = hl.x + 1;
    for (k, d) in git_screen::hints(sc) {
        let need = (k.chars().count() + d.chars().count() + 3) as u16;
        if x + need > hl.right() {
            break;
        }
        x = s.print(x, hl.y, k, ui.text.bold(), hl.right());
        x = s.print(x + 1, hl.y, d, ui.text_muted, hl.right());
        x += 2;
    }

    if let Some(d) = &sc.dialog {
        draw_dialog(ed, s, area, &d.title, &d.lines, &d.keys);
        return CursorShape::Hidden;
    }
    if let Some(p) = &sc.branches {
        return draw_branches(ed, s, area, p);
    }
    CursorShape::Hidden
}

fn section_style(ed: &Editor) -> Style {
    ed.theme.ui.text.patch(ed.theme.diff.hunk).bold()
}

fn draw_status(ed: &Editor, s: &mut Surface, sc: &Screen, area: Rect) {
    let ui = &ed.theme.ui;
    if sc.rows.is_empty() {
        let text = if sc.loading {
            "reading the repository..."
        } else {
            "nothing to show"
        };
        s.print(area.x + 1, area.y, text, ui.text_muted, area.right());
        return;
    }
    let sel = sc.selection();
    for (i, row) in sc
        .rows
        .iter()
        .enumerate()
        .skip(sc.top)
        .take(area.height as usize)
    {
        let y = area.y + (i - sc.top) as u16;
        let r = Rect::new(area.x, y, area.width, 1);
        draw_status_row(ed, s, sc, r, row);
        if i == sc.cursor {
            s.patch(r, ui.cursorline);
        }
        // The selection shows on the cursor row too; the bar marks the
        // cursor.
        if sel.is_some_and(|(a, b)| i >= a && i <= b) {
            s.patch(r, ui.selection);
        }
        if i == sc.cursor {
            s.put(r.x, y, "▌", ui.border_active, r.x + 1);
        }
    }
}

fn draw_status_row(ed: &Editor, s: &mut Surface, sc: &Screen, r: Rect, row: &Row) {
    let ui = &ed.theme.ui;
    let d = &ed.theme.diff;
    let right = r.right();
    let data = sc.data.as_ref();
    match row {
        Row::Head { label, value, warn } => {
            let x = s.print(r.x + 1, r.y, &format!("{label:<12}"), ui.text.bold(), right);
            let style = if *warn { ui.message_warning } else { ui.text };
            s.print(x, r.y, value, style, right);
        }
        Row::Blank => {}
        Row::Section { section, count } => {
            let folded = sc.collapsed.contains(section);
            let text = format!(
                "{} ({count}){}",
                section.title(),
                if folded { ", folded" } else { "" }
            );
            s.print(r.x + 1, r.y, &text, section_style(ed), right);
        }
        Row::Untracked(i) => {
            if let Some(p) = data.and_then(|d| d.untracked.get(*i)) {
                let x = s.print(
                    r.x + 3,
                    r.y,
                    &format!("{:<11}", "untracked"),
                    ui.text_muted,
                    right,
                );
                s.print(x, r.y, &p.display().to_string(), ui.text, right);
            }
        }
        Row::File { section, file } => {
            if let Some(f) = sc.file(*section, *file) {
                let key = (*section, f.target_path().cloned().unwrap_or_default());
                let open = sc.expanded.contains(&key);
                let (add, del) = f.file.counts();
                let label = file_label(f);
                let style = match section {
                    Section::Staged => ui.text.patch(d.git_add),
                    _ if f.file.unmerged => ui.text.patch(d.git_conflict),
                    _ => ui.text.patch(d.git_changed),
                };
                let x = s.print(r.x + 3, r.y, &label, style, right);
                let tail = if f.file.binary || f.file.unmerged {
                    String::new()
                } else {
                    format!("+{add} -{del}")
                };
                let x = s.print(x + 2, r.y, &tail, ui.text_muted, right);
                if !open && !f.file.hunks.is_empty() {
                    s.print(x + 2, r.y, "(tab shows hunks)", ui.text_muted, right);
                }
            }
        }
        Row::Note(t) => {
            s.print(r.x + 3, r.y, t, ui.text_muted, right);
        }
        Row::Hunk { header, .. } => {
            s.print(r.x + 5, r.y, header, d.hunk, right);
        }
        Row::Line { row, .. } => {
            draw_row(
                ed,
                s,
                Rect::new(r.x + 3, r.y, r.width.saturating_sub(3), 1),
                &ReviewRow::Line(row.clone()),
                0,
            );
        }
        Row::Stash(i) => {
            if let Some(st) = data.and_then(|d| d.stashes.get(*i)) {
                let x = s.print(
                    r.x + 3,
                    r.y,
                    &format!("stash@{{{}}}", st.index),
                    ui.text.bold(),
                    right,
                );
                s.print(x + 2, r.y, &st.message, ui.text, right);
            }
        }
        Row::Commit(i) => {
            if let Some(c) = data.and_then(|d| d.recent.get(*i)) {
                let x = s.print(r.x + 3, r.y, &c.short, ui.text.bold(), right);
                let mut x = x + 1;
                if !c.refs.is_empty() {
                    x = s.print(
                        x,
                        r.y,
                        &format!("({})", c.refs),
                        ui.text.patch(d.git_changed),
                        right,
                    ) + 1;
                }
                s.print(x, r.y, &c.subject, ui.text, right);
            }
        }
    }
}

fn draw_detail_row(ed: &Editor, s: &mut Surface, r: Rect, row: &DetailRow) {
    let ui = &ed.theme.ui;
    match row {
        DetailRow::Text(t) => {
            s.print(r.x + 1, r.y, t, ui.text, r.right());
        }
        DetailRow::File(t) => {
            s.print(r.x + 1, r.y, t, ui.text.bold(), r.right());
        }
        DetailRow::Note(t) => {
            s.print(r.x + 3, r.y, t, ui.text_muted, r.right());
        }
        DetailRow::Hunk(h) => {
            draw_row(
                ed,
                s,
                r,
                &ReviewRow::Hunk {
                    index: 0,
                    header: h.clone(),
                },
                0,
            );
        }
        DetailRow::Line(l) => draw_row(ed, s, r, &ReviewRow::Line(l.clone()), 0),
    }
}

fn draw_branches(ed: &Editor, s: &mut Surface, area: Rect, p: &BranchPicker) -> CursorShape {
    let ui = &ed.theme.ui;
    let w = area.width.saturating_sub(4).min(70);
    let x = area.x + (area.width - w) / 2;
    let items = p.items();
    let h = (items as u16 + 3).min(area.height.saturating_sub(2)).max(4);
    let y = area.y + (area.height.saturating_sub(h)) / 3;
    let b = Rect::new(x, y, w, h);
    draw_box(
        s,
        b,
        "switch or create a branch",
        ui.popup,
        ui.popup_border,
        ui.popup_title,
    );
    let qx = s.print(x + 2, y + 1, "> ", ui.popup_title, b.right() - 1);
    let end = s.print(qx, y + 1, &p.query, ui.popup, b.right() - 1);
    let rows = (h - 3) as usize;
    let first = if p.selected >= rows {
        p.selected + 1 - rows
    } else {
        0
    };
    for i in (first..items).take(rows) {
        let ry = y + 2 + (i - first) as u16;
        let style = if i == p.selected {
            ui.popup_selected
        } else {
            ui.popup
        };
        s.fill(Rect::new(x + 1, ry, w - 2, 1), style);
        if let Some(&bi) = p.matches.get(i) {
            let br = &p.branches[bi];
            let mark = if br.current { "* " } else { "  " };
            let lx = s.print(
                x + 2,
                ry,
                &format!("{mark}{}", br.name),
                style,
                b.right() - 1,
            );
            let detail = match &br.upstream {
                Some(u) => format!("{u}  {}", br.subject),
                None => br.subject.clone(),
            };
            let dstyle = if i == p.selected {
                style
            } else {
                ui.popup_muted
            };
            s.print(lx + 2, ry, &detail, dstyle, b.right() - 1);
        } else {
            s.print(
                x + 2,
                ry,
                &format!("  create branch {} from HEAD", p.query.trim()),
                style,
                b.right() - 1,
            );
        }
    }
    CursorShape::Bar(end.min(b.right() - 2), y + 1)
}

/// The hunk or blame popup below its line in the focused view.
pub(crate) fn draw_popup(ed: &Editor, s: &mut Surface) {
    let Some(p) = &ed.git.popup else {
        return;
    };
    let view = ed.view();
    if p.doc != view.doc || p.line < view.top_line || p.line >= view.top_line + view.text_height() {
        return;
    }
    draw_popup_box(ed, s, view, p);
}

fn draw_popup_box(ed: &Editor, s: &mut Surface, view: &View, p: &Popup) {
    let ui = &ed.theme.ui;
    let d = &ed.theme.diff;
    let line_y = view.area.y + (p.line - view.top_line) as u16;
    let max_w = (s.width as usize).saturating_sub(2).min(100);
    if max_w < 16 {
        return;
    }
    let longest = p
        .lines
        .iter()
        .map(|l| unicode_width::UnicodeWidthStr::width(l.text.as_str()) + 2)
        .chain(std::iter::once(p.title.chars().count() + 2))
        .max()
        .unwrap_or(0);
    let w = (longest + 4).clamp(20.min(max_w), max_w) as u16;
    let below = s.height.saturating_sub(line_y + 2);
    let above = line_y.saturating_sub(view.area.y);
    let rows = p.lines.len().max(1) as u16;
    let (room, place_below) = if below >= rows + 2 || below >= above {
        (below, true)
    } else {
        (above, false)
    };
    let h = (rows + 2).min(room).min(24);
    if h < 3 {
        return;
    }
    let y = if place_below { line_y + 1 } else { line_y - h };
    let x = (view.area.x + ed.gutter_width(view)).min(s.width.saturating_sub(w));
    let b = Rect::new(x, y, w, h);
    draw_box(s, b, &p.title, ui.popup, ui.popup_border, ui.popup_title);
    let visible = (h - 2) as usize;
    if p.lines.is_empty() {
        s.print(x + 2, y + 1, "(no lines)", ui.popup_muted, b.right() - 1);
    }
    for (i, l) in p.lines.iter().take(visible).enumerate() {
        let ry = y + 1 + i as u16;
        let truncated = i + 1 == visible && p.lines.len() > visible;
        if truncated {
            s.print(
                x + 2,
                ry,
                &format!("... {} more line(s)", p.lines.len() - i),
                ui.popup_muted,
                b.right() - 1,
            );
            break;
        }
        match l.kind {
            None => {
                s.print(x + 2, ry, &l.text, ui.popup, b.right() - 1);
            }
            Some(kind) => {
                let (sign, fg, bg, mark_bg) = match kind {
                    DiffLineKind::Added => ("+", d.git_add, d.add_bg, d.add_text_bg),
                    DiffLineKind::Removed => ("-", d.git_removed, d.delete_bg, d.delete_text_bg),
                    DiffLineKind::Context => {
                        (" ", ui.popup, ed.theme.background, ed.theme.background)
                    }
                };
                let base = ui.popup.patch(Style::bg(bg));
                s.fill(Rect::new(x + 1, ry, w - 2, 1), base);
                let mut cx = s.print(x + 2, ry, sign, base.patch(fg), b.right() - 1);
                for (at, g) in unicode_segmentation::UnicodeSegmentation::grapheme_indices(
                    l.text.as_str(),
                    true,
                ) {
                    let marked = l.marks.iter().any(|m| at >= m.start && at < m.end);
                    let st = if marked {
                        base.patch(Style::bg(mark_bg))
                    } else {
                        base
                    };
                    let g = if g == "\t" { " " } else { g };
                    let used = s.put(cx, ry, g, st, b.right() - 1);
                    if used == 0 {
                        break;
                    }
                    cx += used;
                }
            }
        }
    }
}

/// The blame column's text for a line, drawn in the gutter's first
/// `width` cells.
pub(crate) fn draw_blame(
    ed: &Editor,
    s: &mut Surface,
    x: u16,
    y: u16,
    width: u16,
    view: &View,
    line: usize,
) {
    if width == 0 {
        return;
    }
    let ui = &ed.theme.ui;
    s.fill(Rect::new(x, y, width, 1), ui.gutter);
    let Some(cell) = git_hunks::blame_cell(ed, view.doc, line) else {
        return;
    };
    let style = if cell.uncommitted {
        ui.gutter.patch(ed.theme.diff.git_add)
    } else {
        ui.gutter
    };
    s.print(x, y, &cell.label(), style, x + width - 1);
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;
