//! Draws the picker's file preview: a header with the path, then numbered
//! lines with the target line highlighted and kept near the middle.

use maestro_editor::Editor;
use maestro_editor::Rect;
use maestro_editor::navigation::preview::Preview;
use unicode_segmentation::UnicodeSegmentation;

use crate::surface::Surface;

pub(crate) fn draw(ed: &Editor, s: &mut Surface, area: Rect, pv: &Preview) {
    let ui = &ed.theme.ui;
    if area.width < 8 || area.height < 2 {
        return;
    }
    let right = area.right();
    let name = maestro_editor::navigation::display_path(&pv.path, &ed.cwd);
    s.print(area.x + 1, area.y, &name, ui.popup_title, right - 1);
    let rows = (area.height - 1) as usize;
    let body_y = area.y + 1;
    let target = pv.target.map(|t| t.saturating_sub(pv.first_line));
    let start = match target {
        Some(t) => t
            .saturating_sub(rows / 2)
            .min(pv.lines.len().saturating_sub(rows)),
        None => 0,
    };
    let last_no = pv.first_line + (start + rows).min(pv.lines.len());
    let num_w = last_no.to_string().len().max(3);
    let mut used = 0;
    for (row, (idx, line)) in pv
        .lines
        .iter()
        .enumerate()
        .skip(start)
        .take(rows)
        .enumerate()
    {
        let y = body_y + row as u16;
        let hit = Some(idx) == target;
        let (num_style, text_style) = if hit {
            (ui.popup_selected, ui.popup_selected)
        } else {
            (ui.popup_muted, ui.popup)
        };
        if hit {
            s.fill(Rect::new(area.x, y, area.width, 1), ui.popup_selected);
        }
        let number = format!("{:>num_w$} ", pv.first_line + idx + 1);
        let x = s.print(area.x + 1, y, &number, num_style, right);
        draw_text(s, x, y, line, text_style, right - 1, ed.tab_width());
        used = row + 1;
    }
    if let Some(msg) = &pv.message
        && used < rows
    {
        s.print(
            area.x + 1,
            body_y + used as u16,
            msg,
            ui.popup_muted,
            right - 1,
        );
    }
}

/// Prints file text: tabs expand to the next stop, other control
/// characters show as `?` so they cannot reach the terminal.
fn draw_text(
    s: &mut Surface,
    x0: u16,
    y: u16,
    text: &str,
    style: maestro_config::Style,
    max_x: u16,
    tab: usize,
) {
    let mut x = x0;
    for g in text.graphemes(true) {
        if g == "\t" {
            let col = (x - x0) as usize;
            let next = (col / tab + 1) * tab;
            for _ in col..next {
                if s.put(x, y, " ", style, max_x) == 0 {
                    return;
                }
                x += 1;
            }
            continue;
        }
        let g = if g.chars().any(char::is_control) {
            "?"
        } else {
            g
        };
        let w = s.put(x, y, g, style, max_x);
        if w == 0 {
            return;
        }
        x += w;
    }
}
