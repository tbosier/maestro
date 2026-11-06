//! Draws the file tree (`space t`) in its column right of the views, and
//! a file's diff from it over the views. Reads the editor; never changes
//! it.
//!
//! A changed file's mark is a letter (`M`, `A`, `D`, `R`, `?`) in the Git
//! sign color for that change; the selected file also shows the word
//! (`modified`). Directories show how many changed files they hold. The
//! selected row uses the picker's selection colors when the tree has the
//! keys, as the sidebar does.

use maestro_config::Style;
use maestro_editor::workspaces::tree::diff::DiffState;
use maestro_editor::workspaces::tree::{self, PathChangeKind, Row, RowKind};
use maestro_editor::{Editor, Rect};

use crate::render::CursorShape;
use crate::surface::Surface;

/// The color of a change's letter.
pub(crate) fn change_style(ed: &Editor, kind: &PathChangeKind) -> Style {
    let d = &ed.theme.diff;
    match kind {
        PathChangeKind::Added | PathChangeKind::Untracked => d.git_add,
        PathChangeKind::Modified | PathChangeKind::Renamed { .. } => d.git_changed,
        PathChangeKind::Deleted => d.git_removed,
    }
}

pub(crate) fn draw(ed: &Editor, s: &mut Surface, area: Rect) {
    let ui = &ed.theme.ui;
    let t = &ed.workspaces.tree;
    let focused = t.has_focus();
    s.fill(area, ui.text);
    // The divider between the views and the tree.
    if let Some(x) = area.x.checked_sub(1) {
        let border = if focused { ui.border_active } else { ui.border };
        for y in area.y..area.bottom() {
            s.put(x, y, "│", border, x + 1);
        }
    }
    if area.height == 0 || area.width == 0 {
        return;
    }
    let [title, detail] = tree::header(ed);
    let title_style = if focused {
        ui.tab_active
    } else {
        ui.tab_inactive
    };
    s.fill(Rect::new(area.x, area.y, area.width, 1), title_style);
    let room = usize::from(area.width);
    s.print(
        area.x,
        area.y,
        &fit(&format!(" {title}"), room),
        title_style,
        area.right(),
    );
    if area.height > 1 {
        s.print(
            area.x,
            area.y + 1,
            &fit(&format!(" {detail}"), room),
            ui.text_muted,
            area.right(),
        );
    }
    if tree::has_footer(area.height) {
        let hint = if focused {
            " ret open  d diff  c changed"
        } else {
            " space t: keys to the tree"
        };
        s.print(area.x, area.bottom() - 1, hint, ui.text_muted, area.right());
    }

    let body = Rect::new(
        area.x,
        area.y + tree::HEADER_ROWS.min(area.height),
        area.width,
        tree::body_height(ed) as u16,
    );
    let rows = tree::rows(ed);
    let selected = tree::selected_index(ed, &rows);
    let first = tree::first_shown(ed, rows.len());
    for (n, (i, row)) in rows
        .iter()
        .enumerate()
        .skip(first)
        .take(body.height as usize)
        .enumerate()
    {
        let y = body.y + n as u16;
        let is_selected = Some(i) == selected;
        if is_selected {
            let fill = if focused {
                ui.popup_selected
            } else {
                ui.text.patch(ui.cursorline)
            };
            s.fill(Rect::new(body.x, y, body.width, 1), fill);
        }
        let mut x = body.x;
        for (text, style) in row_segments(ed, row, is_selected) {
            let style = match (is_selected, focused) {
                (true, true) => ui.popup_selected,
                // Muted text on the current line is under 4.5:1.
                (true, false) if style.fg == ui.text_muted.fg => ui.text.patch(ui.cursorline),
                (true, false) => style.patch(ui.cursorline),
                (false, _) => style,
            };
            x = s.print(x, y, &text, style, body.right());
            if x >= body.right() {
                break;
            }
        }
    }
}

/// `text` cut to `width` columns, ending in `…` when cut.
fn fit(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if unicode_width::UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    let mut out = out.trim_end().to_string();
    out.push('…');
    out
}

/// A row as text and styles: indent, fold mark or change letter, name,
/// and the changed count or the change's word.
fn row_segments(ed: &Editor, row: &Row, selected: bool) -> Vec<(String, Style)> {
    let ui = &ed.theme.ui;
    let indent = " ".repeat(1 + 2 * row.depth);
    match &row.kind {
        RowKind::Note(text) => vec![(format!("{indent}  {text}"), ui.text_muted)],
        RowKind::Dir { expanded, changed } => {
            let mark = if *expanded { "v" } else { ">" };
            let mut out = vec![
                (format!("{indent}{mark} "), ui.text_muted),
                (format!("{}/", row.name), ui.text.patch(ui.file_dir)),
            ];
            if *changed > 0 {
                out.push((
                    format!(" ({changed})"),
                    ui.text.patch(ed.theme.diff.git_changed),
                ));
            }
            out
        }
        RowKind::File { change } => match change {
            None => vec![(format!("{indent}  {}", row.name), ui.text)],
            Some(kind) => {
                let style = ui.text.patch(change_style(ed, kind));
                let name_style = if *kind == PathChangeKind::Deleted {
                    ui.text_muted
                } else {
                    ui.text
                };
                let mut out = vec![
                    (format!("{indent}{} ", kind.letter()), style.bold()),
                    (row.name.clone(), name_style),
                ];
                if selected {
                    out.push((format!("  {}", kind.word()), style));
                }
                out
            }
        },
    }
}

/// A file's diff (`d` in the tree) over the editor area.
pub(crate) fn draw_diff(ed: &Editor, s: &mut Surface) -> CursorShape {
    let area = ed.area;
    let ui = &ed.theme.ui;
    let Some(d) = &ed.workspaces.tree.diff else {
        return CursorShape::Hidden;
    };
    s.fill(area, ui.text);
    if area.width < 20 || area.height < 4 {
        s.print(
            area.x,
            area.y,
            "diff: the window is too small",
            ui.message_warning,
            area.right(),
        );
        return CursorShape::Hidden;
    }
    let h = Rect::new(area.x, area.y, area.width, 1);
    s.fill(h, ui.statusline);
    let state = match d.state {
        DiffState::Loading => " loading ",
        _ => " KEYS: TREE DIFF ",
    };
    let sw = state.chars().count() as u16;
    let right = h.right().saturating_sub(sw + 1);
    let x = s.print(
        h.x,
        h.y,
        &format!(" diff: {} ", d.title),
        ui.statusline.bold(),
        right,
    );
    let state_style = if matches!(d.state, DiffState::Loading) {
        ui.mode_insert
    } else {
        ui.mode_normal
    };
    s.print(
        h.right().saturating_sub(sw).max(x + 1),
        h.y,
        state,
        state_style,
        h.right(),
    );

    let content = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        tree::diff::content_height(ed) as u16,
    );
    match &d.state {
        DiffState::Loading => {
            s.print(
                content.x + 2,
                content.y,
                "reading the diff…",
                ui.text_muted,
                content.right(),
            );
        }
        DiffState::Failed(e) => {
            s.print(
                content.x + 2,
                content.y,
                &format!("could not read the diff: {e}"),
                ui.message_error,
                content.right(),
            );
        }
        DiffState::Ready { .. } => {
            for (i, row) in d
                .rows()
                .iter()
                .enumerate()
                .skip(d.top)
                .take(content.height as usize)
            {
                let y = content.y + (i - d.top) as u16;
                let r = Rect::new(content.x, y, content.width, 1);
                crate::agents::draw_row(ed, s, r, row, 0);
                if i == d.cursor {
                    // A mark in the free first column: a background over
                    // the row would hide its changed-word marks, and muted
                    // line numbers on the current-line color are 4.36:1.
                    s.print(r.x, y, ">", ui.text.bold(), r.right());
                }
            }
        }
    }

    let hl = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    s.fill(hl, ui.text);
    let mut x = hl.x + 1;
    let layout = if d.side_by_side {
        "unified"
    } else {
        "side by side"
    };
    for (k, what) in [
        ("j/k", "move"),
        ("s", layout),
        ("ret", "open here"),
        ("q", "back to the tree"),
    ] {
        let need = (k.chars().count() + what.chars().count() + 3) as u16;
        if x + need > hl.right() {
            break;
        }
        x = s.print(x, hl.y, k, ui.text.bold(), hl.right());
        x = s.print(x + 1, hl.y, what, ui.text_muted, hl.right());
        x += 2;
    }
    CursorShape::Hidden
}

#[cfg(test)]
#[path = "tree_tests.rs"]
mod tests;
