//! Draws editor state into a [`Surface`]. Reads the editor; never changes it.

use maestro_config::{LineNumbers, Style, Theme};
use maestro_core::text::{grapheme_width, is_line_ending_str, next_grapheme_boundary};
use maestro_editor::editor::Buffer;
use maestro_editor::lsp::{self, DiagnosticSeverity};
use maestro_editor::{Editor, Mode, Presentation, Rect, Severity, View};

use crate::surface::Surface;

/// Where to put the terminal's own cursor, and in which shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Hidden,
    Bar(u16, u16),
}

/// The screen regions the front end gives the editor.
pub fn split_screen(width: u16, height: u16, ed: &Editor) -> (Option<Rect>, Rect, Rect) {
    let tabline = ed.presentation != Presentation::Focus && height > 3;
    let top = u16::from(tabline);
    let views = Rect::new(0, top, width, height.saturating_sub(top + 1));
    let cmdline = Rect::new(0, height.saturating_sub(1), width, 1);
    (tabline.then(|| Rect::new(0, 0, width, 1)), views, cmdline)
}

pub fn render(ed: &Editor, surface: &mut Surface) -> CursorShape {
    let theme = &ed.theme;
    surface.reset(theme.ui.text);
    let (tabline, _, cmdline) = split_screen(surface.width, surface.height, ed);
    if let Some(area) = tabline {
        draw_tabline(ed, surface, area);
    }
    let placements = ed.layout.compute(ed.area);
    for p in &placements {
        if let Some(view) = ed.views.get(&p.view) {
            draw_view(ed, view, surface);
        }
    }
    draw_dividers(ed, surface, &placements);
    draw_hover(ed, surface);
    draw_signature(ed, surface);
    draw_completion(ed, surface);
    let mut cursor = CursorShape::Hidden;
    if ed.proposals.review_open() {
        // The proposal review covers the editor area (crate::proposals).
        cursor = crate::proposals::draw(ed, surface);
    } else if ed.agents.panel_open() {
        // The agent panel covers the editor area (crate::agents).
        cursor = crate::agents::draw(ed, surface);
    } else if ed.mode == Mode::Insert && ed.prompt.is_none() && ed.picker.is_none() {
        cursor = insert_cursor(ed, ed.view());
    }
    if let Some(c) = draw_cmdline(ed, surface, cmdline) {
        cursor = c;
    }
    draw_which_key(ed, surface, cmdline);
    if ed.picker.is_some()
        && let Some(c) = draw_picker(ed, surface)
    {
        cursor = c;
    }
    cursor
}

fn draw_tabline(ed: &Editor, s: &mut Surface, area: Rect) {
    let ui = &ed.theme.ui;
    s.fill(area, ui.tabline);
    let current = ed.view().doc;
    let mut x = area.x;
    for b in ed.buffers.values() {
        let name = b
            .doc
            .path()
            .map(|p| p.strip_prefix(&ed.cwd).unwrap_or(p).display().to_string())
            .unwrap_or_else(|| "[scratch]".into());
        let modified = if b.doc.is_modified() { " +" } else { "" };
        let style = if b.id == current {
            ui.tab_active
        } else {
            ui.tab_inactive
        };
        let label = format!(" {name}{modified} ");
        x = s.print(x, area.y, &label, style, area.right());
        if x >= area.right() {
            break;
        }
    }
}

fn draw_dividers(ed: &Editor, s: &mut Surface, placements: &[maestro_editor::layout::Placement]) {
    let ui = &ed.theme.ui;
    for p in placements {
        let r = p.rect;
        let x = r.right();
        if x >= ed.area.right() {
            continue;
        }
        // Only draw where no other view sits (the gap left by the layout).
        for y in r.y..r.bottom() {
            let occupied = placements.iter().any(|q| q.rect.contains(x, y));
            if !occupied {
                let style = if p.view == ed.focus {
                    ui.border_active
                } else {
                    ui.border
                };
                s.put(x, y, "│", style, x + 1);
            }
        }
    }
}

/// Per-line selection spans, as `(from, to_exclusive, is_primary, cursor)`.
struct Spans {
    ranges: Vec<(usize, usize, bool, usize)>,
}

fn selection_spans(buf: &Buffer, view: &View, insert: bool) -> Spans {
    let text = buf.doc.text().slice(..);
    let primary = view.selection.primary_index();
    let ranges = view
        .selection
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let (from, to) = if insert {
                (r.head, r.head)
            } else {
                (r.from(), r.to(text))
            };
            (from, to, i == primary, r.head)
        })
        .collect();
    Spans { ranges }
}

fn draw_view(ed: &Editor, view: &View, s: &mut Surface) {
    let theme = &ed.theme;
    let ui = &theme.ui;
    let buf = &ed.buffers[&view.doc];
    let text = buf.doc.text().slice(..);
    let focused = view.id == ed.focus;
    let area = view.area;
    let gutter = ed.gutter_width(view);
    let text_h = view.text_height();
    let text_x = area.x + gutter;
    let right = area.right();
    let tab = ed.tab_width();
    let insert = focused && ed.mode == Mode::Insert;
    let spans = selection_spans(buf, view, insert);
    let last_line = maestro_editor::motion::last_line(text);
    let primary_line = text.char_to_line(view.selection.primary().head.min(text.len_chars()));
    let cursorline =
        ed.config.editor.cursorline && focused && ed.presentation != Presentation::Focus;
    let matches = search_matches(ed, view, text_h);
    let signs = lsp::line_signs(ed, view.doc, view.top_line..view.top_line + text_h);
    let diag_spans = {
        let first = text.line_to_char(view.top_line.min(text.len_lines()));
        let last = text.line_to_char((view.top_line + text_h).min(text.len_lines()));
        lsp::spans_in(ed, view.doc, first, last.max(first + 1))
    };
    let syntax = syntax_styles(ed, buf, view, text_h);
    let mut next_syntax = 0;

    s.fill(
        Rect::new(area.x, area.y, area.width, text_h as u16),
        ui.text,
    );
    for row in 0..text_h {
        let line = view.top_line + row;
        let y = area.y + row as u16;
        if line > last_line {
            continue;
        }
        let base = if cursorline && line == primary_line {
            ui.text.patch(ui.cursorline)
        } else {
            ui.text
        };
        if base != ui.text {
            s.fill(
                Rect::new(text_x, y, area.width.saturating_sub(gutter), 1),
                base,
            );
        }
        draw_gutter(ed, s, area.x, y, gutter, line, primary_line, focused);
        // One sign cell, three sources. Errors and warnings win; a Git
        // change wins over an info or hint, which is drawn only when the
        // line has no Git sign.
        let diag = signs.get(&line).copied();
        let urgent = |sev: &DiagnosticSeverity| {
            matches!(sev, DiagnosticSeverity::Error | DiagnosticSeverity::Warning)
        };
        if let Some(sev) = diag.filter(|d| !urgent(d)) {
            draw_diagnostic_sign(ed, s, area.x, y, gutter, sev);
        }
        draw_git_sign(ed, s, area.x, y, gutter, view, line, primary_line);
        if let Some(sev) = diag.filter(urgent) {
            draw_diagnostic_sign(ed, s, area.x, y, gutter, sev);
        }

        // Text.
        let line_start = text.line_to_char(line);
        let line_end = text.line_to_char((line + 1).min(text.len_lines()));
        let mut pos = line_start;
        let mut col = 0usize;
        let mut x = text_x;
        let line_spans: Vec<(usize, usize, bool, usize)> = spans
            .ranges
            .iter()
            .filter(|(f, t, _, h)| {
                (*f < line_end.max(line_start + 1) && *t > line_start)
                    || (*h >= line_start && *h <= line_end)
            })
            .copied()
            .collect();
        while pos < line_end || (pos == line_end && line == last_line && pos == text.len_chars()) {
            let at_eof = pos >= text.len_chars();
            let next = if at_eof {
                pos + 1
            } else {
                next_grapheme_boundary(text, pos)
            };
            let g_owned;
            let g: &str = if at_eof {
                " "
            } else {
                let slice = text.slice(pos..next);
                match slice.as_str() {
                    Some(st) => st,
                    None => {
                        g_owned = slice.to_string();
                        &g_owned
                    }
                }
            };
            let eol = is_line_ending_str(g) || at_eof;
            let w = if eol { 1 } else { grapheme_width(g, col, tab) };
            if col + w > view.left_col && x < right {
                let mut style = base;
                while syntax.get(next_syntax).is_some_and(|&(_, to, _)| to <= pos) {
                    next_syntax += 1;
                }
                if let Some(&(from, _, st)) = syntax.get(next_syntax)
                    && from <= pos
                {
                    style = style.patch(st);
                }
                if let Some(&(_, _, sev)) = diag_spans
                    .iter()
                    .filter(|(a, b, _)| pos >= *a && pos < *b)
                    .min_by_key(|(_, _, sev)| *sev)
                {
                    style = style.patch(diagnostic_range_style(theme, sev));
                }
                if let Some(m) = &matches
                    && m.iter().any(|&(a, b)| pos >= a && pos < b)
                {
                    style = style.patch(ui.search_match);
                }
                for &(from, to, primary, head) in &line_spans {
                    if pos >= from && pos < to {
                        style = style.patch(if primary {
                            ui.selection_primary
                        } else {
                            ui.selection
                        });
                    }
                    if focused && !insert && pos == head {
                        style = style.patch(if primary {
                            ui.cursor
                        } else {
                            ui.cursor_secondary
                        });
                    } else if insert && pos == head && !primary {
                        style = style.patch(ui.cursor_secondary);
                    }
                }
                let visible_from = col.max(view.left_col);
                let draw_x = text_x + (visible_from - view.left_col) as u16;
                if g == "\t" || eol {
                    let cells = (col + w - visible_from) as u16;
                    for k in 0..cells {
                        s.put(draw_x + k, y, " ", style, right);
                    }
                } else if g.chars().next().is_some_and(char::is_control) {
                    let c = g.chars().next().unwrap_or('?') as u32;
                    let caret = char::from_u32((c + 64) & 0x7f).unwrap_or('?');
                    let ctrl_style = style.patch(Style::fg(theme_special(theme)));
                    s.put(draw_x, y, "^", ctrl_style, right);
                    s.put(draw_x + 1, y, &caret.to_string(), ctrl_style, right);
                } else if col >= view.left_col {
                    if s.put(draw_x, y, g, style, right) == 0 {
                        // A wide grapheme cut by the right edge.
                        s.put(draw_x, y, " ", style, right);
                    }
                } else {
                    // Left half scrolled off.
                    s.put(draw_x, y, " ", style, right);
                }
                x = draw_x + (col + w - visible_from) as u16;
            }
            col += w;
            if x >= right || eol {
                break;
            }
            pos = next;
        }
    }
    draw_statusline(
        ed,
        view,
        s,
        Rect::new(area.x, area.y + text_h as u16, area.width, 1),
    );
}

fn theme_special(theme: &Theme) -> maestro_config::Rgb {
    theme
        .syntax("constant.character.escape")
        .and_then(|s| s.fg)
        .unwrap_or(maestro_config::duskfox::YELLOW.bright)
}

#[allow(clippy::too_many_arguments)]
fn draw_gutter(
    ed: &Editor,
    s: &mut Surface,
    x: u16,
    y: u16,
    width: u16,
    line: usize,
    primary_line: usize,
    focused: bool,
) {
    if width == 0 {
        return;
    }
    let ui = &ed.theme.ui;
    let current = line == primary_line && focused;
    let style = if current {
        ui.gutter_current
    } else {
        ui.gutter
    };
    s.fill(Rect::new(x, y, width, 1), style);
    let n = match ed.config.editor.line_numbers {
        LineNumbers::Off => return,
        LineNumbers::Absolute => line + 1,
        LineNumbers::Relative if current => line + 1,
        LineNumbers::Relative => line.abs_diff(primary_line),
    };
    let label = format!("{n:>w$} ", w = width as usize - 2);
    s.print(x + 1, y, &label, style, x + width);
}

// ----- git sign column (maestro-git) -----------------------------------
// Draws `+`, `~` or `-` in the gutter's first cell, colored by the theme's
// diff roles. The symbol carries the meaning; color only reinforces it.
#[allow(clippy::too_many_arguments)]
fn draw_git_sign(
    ed: &Editor,
    s: &mut Surface,
    x: u16,
    y: u16,
    width: u16,
    view: &View,
    line: usize,
    primary_line: usize,
) {
    if width == 0 {
        return;
    }
    let Some(sign) = ed.git.sign_at(view.doc, line) else {
        return;
    };
    let diff = &ed.theme.diff;
    let color = match sign {
        maestro_editor::git::Sign::Added => diff.git_add,
        maestro_editor::git::Sign::Changed => diff.git_changed,
        maestro_editor::git::Sign::Removed => diff.git_removed,
    };
    let ui = &ed.theme.ui;
    let gutter = if line == primary_line && view.id == ed.focus {
        ui.gutter_current
    } else {
        ui.gutter
    };
    s.put(x, y, sign.symbol(), gutter.patch(color), x + 1);
}
// ----- end git sign column ---------------------------------------------

fn diagnostic_fg(theme: &Theme, sev: DiagnosticSeverity) -> Style {
    let d = &theme.diagnostic;
    match sev {
        DiagnosticSeverity::Error => d.error,
        DiagnosticSeverity::Warning => d.warning,
        DiagnosticSeverity::Info => d.info,
        DiagnosticSeverity::Hint => d.hint,
    }
}

/// Text inside a diagnostic range: a severity tint plus an underline, so
/// the range shows without relying on hue alone.
fn diagnostic_range_style(theme: &Theme, sev: DiagnosticSeverity) -> Style {
    let d = &theme.diagnostic;
    let tint = match sev {
        DiagnosticSeverity::Error => d.error_bg,
        DiagnosticSeverity::Warning => d.warning_bg,
        DiagnosticSeverity::Info => d.info_bg,
        DiagnosticSeverity::Hint => d.hint_bg,
    };
    Style::bg(tint).underline()
}

/// The severity letter in the gutter's sign column (its first cell). It
/// keeps the plain gutter background even on the cursor line, where the
/// severity colors are checked for contrast.
fn draw_diagnostic_sign(
    ed: &Editor,
    s: &mut Surface,
    x: u16,
    y: u16,
    gutter: u16,
    sev: DiagnosticSeverity,
) {
    if gutter == 0 {
        return;
    }
    let style = ed
        .theme
        .ui
        .gutter
        .patch(diagnostic_fg(&ed.theme, sev))
        .bold();
    s.put(x, y, &sev.letter().to_string(), style, x + 1);
}

/// Hover text in a box below the cursor (above it when there is no room),
/// wrapped to the box width.
fn draw_hover(ed: &Editor, s: &mut Surface) {
    let Some(hover) = &ed.lsp.hover else {
        return;
    };
    let view = ed.view();
    if hover.doc != view.doc {
        return;
    }
    let ui = &ed.theme.ui;
    let text = ed.doc().text().slice(..);
    let anchor = hover.anchor.min(text.len_chars());
    let line = text.char_to_line(anchor);
    if line < view.top_line || line >= view.top_line + view.text_height() {
        return;
    }
    let cursor_y = view.area.y + (line - view.top_line) as u16;
    let col = maestro_editor::motion::display_col(text, anchor, ed.tab_width());
    let cursor_x = (view.area.x + ed.gutter_width(view) + col.saturating_sub(view.left_col) as u16)
        .min(s.width.saturating_sub(1));

    let max_w = (s.width as usize).saturating_sub(2).min(80);
    if max_w < 12 {
        return;
    }
    let inner = max_w - 4;
    let mut rows: Vec<String> = Vec::new();
    for l in &hover.lines {
        rows.extend(wrap(l, inner));
    }
    let longest = rows
        .iter()
        .map(|r| unicode_width::UnicodeWidthStr::width(r.as_str()))
        .max()
        .unwrap_or(0);
    let w = (longest + 4).clamp(16.min(max_w), max_w) as u16;
    let below = s.height.saturating_sub(cursor_y + 2);
    let above = cursor_y;
    let (room, place_below) = if below >= 4 || below >= above {
        (below, true)
    } else {
        (above, false)
    };
    let h = (rows.len() as u16 + 2).min(room).min(20);
    if h < 3 {
        return;
    }
    let y = if place_below {
        cursor_y + 1
    } else {
        cursor_y - h
    };
    let x = cursor_x.min(s.width - w);
    let area = Rect::new(x, y, w, h);
    draw_box(s, area, "hover", ui.popup, ui.popup_border, ui.popup_title);
    let visible = (h - 2) as usize;
    for (i, row) in rows.iter().take(visible).enumerate() {
        let truncated = i + 1 == visible && rows.len() > visible;
        let shown = if truncated { "…" } else { row.as_str() };
        s.print(x + 2, y + 1 + i as u16, shown, ui.popup, area.right() - 1);
    }
}

/// Signature help: the active signature on one row in a box above the
/// cursor line (below it when there is no room above), so it does not
/// cover the completion popup, which opens below. The active parameter is
/// bold and underlined as well as colored. A label too long for the box is
/// cut with `…`, from the left if that keeps the active parameter visible.
fn draw_signature(ed: &Editor, s: &mut Surface) {
    use maestro_editor::lsp::signature;
    use unicode_width::UnicodeWidthChar;
    let Some(popup) = signature::visible(ed) else {
        return;
    };
    let Some(sig) = popup.help.signatures.get(popup.help.active) else {
        return;
    };
    let view = ed.view();
    let ui = &ed.theme.ui;
    let text = ed.doc().text().slice(..);
    let head = view.selection.primary().head.min(text.len_chars());
    let line = text.char_to_line(head);
    if line < view.top_line || line >= view.top_line + view.text_height() {
        return;
    }
    let cursor_y = view.area.y + (line - view.top_line) as u16;
    let col = maestro_editor::motion::display_col(text, head, ed.tab_width());
    let cursor_x = (view.area.x + ed.gutter_width(view) + col.saturating_sub(view.left_col) as u16)
        .min(s.width.saturating_sub(1));
    let max_w = s.width as usize;
    if max_w < 10 || s.height < 3 {
        return;
    }
    let chars: Vec<char> = sig.label.chars().collect();
    let width_of = |cs: &[char]| cs.iter().map(|c| c.width().unwrap_or(0)).sum::<usize>();
    let inner = max_w - 4;
    // Which chars to show: all of them, or a window that ends at the
    // active parameter when it would fall off the right edge.
    let mut start = 0;
    if width_of(&chars) > inner
        && let Some((_, end)) = sig.active
        && width_of(&chars[..end.min(chars.len())]) > inner - 1
    {
        while start < chars.len() && 1 + width_of(&chars[start..end.min(chars.len())]) > inner - 1 {
            start += 1;
        }
    }
    let title = if popup.help.signatures.len() > 1 {
        format!(
            "signature {}/{}",
            popup.help.active + 1,
            popup.help.signatures.len()
        )
    } else {
        "signature".to_string()
    };
    let shown = width_of(&chars[start..]) + usize::from(start > 0);
    let w = (shown.min(inner) + 4).max(title.len() + 6).min(max_w) as u16;
    let y = if cursor_y >= 3 {
        cursor_y - 3
    } else if cursor_y + 4 <= s.height.saturating_sub(1) {
        cursor_y + 1
    } else {
        return;
    };
    let x = cursor_x.saturating_sub(2).min(s.width - w);
    let area = Rect::new(x, y, w, 3);
    draw_box(s, area, &title, ui.popup, ui.popup_border, ui.popup_title);
    let right = area.right() - 1;
    let mut cx = x + 2;
    if start > 0 {
        cx += s.put(cx, y + 1, "…", ui.popup_muted, right);
    }
    let active = ui.popup.patch(ui.popup_match).bold().underline();
    for (i, ch) in chars.iter().enumerate().skip(start) {
        let style = match sig.active {
            Some((a, b)) if a <= i && i < b => active,
            _ => ui.popup,
        };
        // Leave the last column for `…` when the label goes on.
        let last = i + 1 == chars.len();
        let room = right.saturating_sub(cx);
        let cw = ch.width().unwrap_or(0) as u16;
        if !last && room <= cw {
            s.put(cx, y + 1, "…", ui.popup_muted, right);
            break;
        }
        let used = s.put(cx, y + 1, &ch.to_string(), style, right);
        if used == 0 && cw > 0 {
            break;
        }
        cx += used;
    }
}

/// The completion popup: one row per matching item with its label, kind
/// and detail, under the identifier being completed (above it when there
/// is not room below). The title shows the selected row's number.
fn draw_completion(ed: &Editor, s: &mut Surface) {
    use maestro_editor::lsp::completion::{self, MAX_VISIBLE, MIN_AREA};
    use unicode_width::UnicodeWidthStr;
    let Some(menu) = completion::visible(ed) else {
        return;
    };
    let view = ed.view();
    let ui = &ed.theme.ui;
    let text = ed.doc().text().slice(..);
    let start = menu.start.min(text.len_chars());
    let line = text.char_to_line(start);
    if line < view.top_line || line >= view.top_line + view.text_height() {
        return;
    }
    let row_y = view.area.y + (line - view.top_line) as u16;
    let col = maestro_editor::motion::display_col(text, start, ed.tab_width());
    let word_x = view.area.x as usize + ed.gutter_width(view) as usize;
    let word_x = (word_x + col.saturating_sub(view.left_col)) as u16;

    let shown = menu.matches.len().min(MAX_VISIBLE);
    let items = || menu.matches.iter().map(|m| &menu.items[m.index]);
    let label_w = items().map(|i| i.label.width()).max().unwrap_or(0).min(40);
    let kind_w = items().map(|i| i.kind.width()).max().unwrap_or(0);
    let detail_w = items().map(|i| i.detail.width()).max().unwrap_or(0).min(40);
    let gap = |w: usize| if w > 0 { w + 2 } else { 0 };
    let inner = 1 + label_w + gap(kind_w) + gap(detail_w) + 1;
    let w = ((inner + 2) as u16).max(MIN_AREA.0).min(s.width);
    if w < MIN_AREA.0 {
        return;
    }
    let below = s.height.saturating_sub(row_y + 2);
    let above = row_y;
    let want = shown as u16 + 2;
    let (room, place_below) = if below >= want || below >= above {
        (below, true)
    } else {
        (above, false)
    };
    let h = want.min(room);
    if h < 3 {
        return;
    }
    let y = if place_below { row_y + 1 } else { row_y - h };
    // The label column lines up with the identifier being completed.
    let x = word_x.saturating_sub(2).min(s.width - w);
    let area = Rect::new(x, y, w, h);
    let title = format!("{}/{}", menu.selected + 1, menu.matches.len());
    draw_box(s, area, &title, ui.popup, ui.popup_border, ui.popup_title);
    let rows = (h - 2) as usize;
    let scroll = (menu.selected + 1).saturating_sub(rows);
    let right = area.right() - 1;
    for (row, m) in menu.matches.iter().skip(scroll).take(rows).enumerate() {
        let item = &menu.items[m.index];
        let ry = y + 1 + row as u16;
        let selected = scroll + row == menu.selected;
        let (base, hl, muted) = if selected {
            (
                ui.popup_selected,
                ui.popup_selected.patch(ui.popup_match_selected),
                ui.popup_selected,
            )
        } else {
            (ui.popup, ui.popup.patch(ui.popup_match), ui.popup_muted)
        };
        s.fill(Rect::new(x + 1, ry, w - 2, 1), base);
        let label_r = (x + 2 + label_w as u16).min(right);
        let mut cx = x + 2;
        for (i, ch) in item.label.chars().enumerate() {
            let st = if m.positions.contains(&i) { hl } else { base };
            let used = s.put(cx, ry, &ch.to_string(), st, label_r);
            if used == 0 {
                break;
            }
            cx += used;
        }
        let mut cx = label_r;
        if kind_w > 0 {
            cx += 2;
            s.print(cx, ry, item.kind, muted, right);
            cx += kind_w as u16;
        }
        if detail_w > 0 && cx + 2 < right {
            cx += 2;
            let room = (right - cx) as usize;
            let detail = if item.detail.width() > room {
                let mut d: String = String::new();
                for ch in item.detail.chars() {
                    if d.width() + ch.to_string().width() + 1 > room {
                        break;
                    }
                    d.push(ch);
                }
                d.push('…');
                d
            } else {
                item.detail.clone()
            };
            s.print(cx, ry, &detail, muted, right);
        }
    }
}

/// Splits `line` into pieces at most `width` columns wide, preferring
/// spaces as break points.
pub(crate) fn wrap(line: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    for ch in line.chars() {
        let cw = ch.width().unwrap_or(0);
        if cur_w + cw > width && !cur.is_empty() {
            if ch == ' ' {
                // Break here and drop the space.
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
                continue;
            }
            match cur.rfind(' ').filter(|&i| i > 0) {
                Some(i) => {
                    let rest = cur.split_off(i + 1);
                    out.push(cur.trim_end().to_string());
                    cur = rest;
                }
                None => out.push(std::mem::take(&mut cur)),
            }
            cur_w = unicode_width::UnicodeWidthStr::width(cur.as_str());
        }
        cur.push(ch);
        cur_w += cw;
    }
    out.push(cur);
    out
}

/// Syntax styles for the visible lines, as sorted, non-overlapping
/// `(from, to, style)` char spans.
fn syntax_styles(
    ed: &Editor,
    buf: &Buffer,
    view: &View,
    height: usize,
) -> Vec<(usize, usize, Style)> {
    let text = buf.doc.text();
    let first = view.top_line.min(text.len_lines().saturating_sub(1));
    let last = (view.top_line + height).min(text.len_lines());
    let (from, to) = (text.line_to_char(first), text.line_to_char(last));
    // Very long lines go uncolored: highlighting costs time and two bytes
    // of scratch memory per byte of the visible lines.
    if to - from > 256 * 1024 {
        return Vec::new();
    }
    buf.syntax
        .highlights(text, from..to)
        .into_iter()
        .filter_map(|(a, b, scope)| ed.theme.syntax(scope).map(|st| (a, b, st)))
        .collect()
}

fn search_matches(ed: &Editor, view: &View, height: usize) -> Option<Vec<(usize, usize)>> {
    let pattern = ed.last_search.as_ref()?;
    let re = maestro_editor::search::compile(pattern).ok()?;
    let text = ed.buffers[&view.doc].doc.text();
    let first = view.top_line.min(text.len_lines().saturating_sub(1));
    let last = (view.top_line + height).min(text.len_lines());
    let from = text.line_to_char(first);
    let to = text.line_to_char(last);
    // Bound the work for pathological lines.
    if to - from > 1_000_000 {
        return None;
    }
    Some(maestro_editor::search::matches_in(text, &re, from, to))
}

fn draw_statusline(ed: &Editor, view: &View, s: &mut Surface, area: Rect) {
    let ui = &ed.theme.ui;
    let focused = view.id == ed.focus;
    let base = if focused {
        ui.statusline
    } else {
        ui.statusline_inactive
    };
    s.fill(area, base);
    let buf = &ed.buffers[&view.doc];
    let doc = &buf.doc;
    let text = doc.text().slice(..);

    // Right side first: position and file facts always stay visible; the
    // file name gives way when space runs out.
    let head = view.selection.primary().head.min(text.len_chars());
    let line = text.char_to_line(head);
    let col = maestro_editor::motion::display_col(text, head, ed.tab_width());
    let sels = view.selection.len();
    let mut right = String::new();
    if ed.presentation != Presentation::Focus {
        if sels > 1 {
            right.push_str(&format!(
                "{}/{} sel  ",
                view.selection.primary_index() + 1,
                sels
            ));
        }
        // The agent whose worktree holds this file (crate::agents).
        if let Some(a) = doc.path().and_then(|p| ed.agents.owner_of(p)) {
            right.push_str(&format!("agent {} worktree  ", a.name));
        }
        // Git branch (maestro-git), shortened so it never crowds out the rest.
        if let Some(branch) = ed.git.branch_label(view.doc) {
            let chars: Vec<char> = branch.chars().collect();
            let shown: String = if chars.len() > 24 {
                chars[..23].iter().chain(['…'].iter()).collect()
            } else {
                branch
            };
            right.push_str(&format!("{shown}  "));
        }
        right.push_str(&format!(
            "{}  {}  ",
            doc.encoding().label(),
            doc.line_ending().label()
        ));
    }
    right.push_str(&format!("{}:{} ", line + 1, col + 1));
    let right_w = right.chars().count() as u16;
    let left_limit = if area.width > right_w + 12 {
        let muted = if focused { ui.statusline_muted } else { base };
        s.print(area.right() - right_w, area.y, &right, muted, area.right());
        area.right() - right_w - 1
    } else {
        area.right()
    };

    let mut x = area.x;
    if focused && ed.presentation != Presentation::Focus {
        let (label, style) = match (ed.prompt.is_some(), ed.mode) {
            (true, _) => ("CMD", ui.mode_command),
            (_, Mode::Normal) => ("NOR", ui.mode_normal),
            (_, Mode::Insert) => ("INS", ui.mode_insert),
            (_, Mode::Select) => ("SEL", ui.mode_select),
        };
        x = s.print(x, area.y, &format!(" {label} "), style, left_limit);
    }
    let mut flags = String::new();
    if doc.is_modified() {
        flags.push_str(" [+]");
    }
    if let Some(r) = doc.read_only() {
        flags.push_str(&format!(" [read-only: {}]", r.describe()));
    }
    if focused && let Some((reg, _)) = &ed.recording {
        flags.push_str(&format!("  recording @{reg}"));
    }
    // Edit proposals waiting for the user (crate::proposals), on every
    // focused status line until they are decided.
    let waiting = ed.proposals.pending_count();
    if focused && waiting > 0 {
        flags.push_str(&format!(
            "  {waiting} proposal{} waiting: space e",
            if waiting == 1 { "" } else { "s" }
        ));
    }
    let name = doc
        .path()
        .map(|p| p.strip_prefix(&ed.cwd).unwrap_or(p).display().to_string())
        .unwrap_or_else(|| "[scratch]".into());
    let avail = left_limit.saturating_sub(x + 1 + flags.chars().count() as u16) as usize;
    let name_chars: Vec<char> = name.chars().collect();
    let name = if name_chars.len() > avail && avail > 1 {
        let tail: String = name_chars[name_chars.len() - (avail - 1)..]
            .iter()
            .collect();
        format!("…{tail}")
    } else {
        name
    };
    x = s.print(x, area.y, &format!(" {name}"), base, left_limit);
    if !flags.is_empty() {
        s.print(x, area.y, &flags, base.patch(ui.file_modified), left_limit);
    }
}

fn insert_cursor(ed: &Editor, view: &View) -> CursorShape {
    let text = ed.buffers[&view.doc].doc.text().slice(..);
    let head = view.selection.primary().head.min(text.len_chars());
    let line = text.char_to_line(head);
    if line < view.top_line || line >= view.top_line + view.text_height() {
        return CursorShape::Hidden;
    }
    let col = maestro_editor::motion::display_col(text, head, ed.tab_width());
    if col < view.left_col {
        return CursorShape::Hidden;
    }
    let x = view.area.x + ed.gutter_width(view) + (col - view.left_col) as u16;
    let y = view.area.y + (line - view.top_line) as u16;
    if x >= view.area.right() {
        return CursorShape::Hidden;
    }
    CursorShape::Bar(x, y)
}

fn draw_cmdline(ed: &Editor, s: &mut Surface, area: Rect) -> Option<CursorShape> {
    let ui = &ed.theme.ui;
    s.fill(area, ui.text);
    if let Some(p) = &ed.prompt {
        let label = p.kind.label();
        let x = s.print(
            area.x,
            area.y,
            label,
            ui.prompt.patch(Style::default().bold()),
            area.right(),
        );
        // Scroll long input so the cursor stays visible.
        let avail = area.right().saturating_sub(x + 1) as usize;
        let before: String = p.input[..p.cursor].to_string();
        let before_w = unicode_width::UnicodeWidthStr::width(before.as_str());
        let skip = before_w.saturating_sub(avail);
        let mut shown = String::new();
        let mut acc = 0;
        for ch in p.input.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if acc >= skip {
                shown.push(ch);
            }
            acc += cw;
        }
        s.print(x, area.y, &shown, ui.prompt, area.right());
        if let Some(fb) = &p.feedback {
            let w = fb.chars().count() as u16 + 1;
            if area.width > w + 20 {
                s.print(
                    area.right() - w,
                    area.y,
                    fb,
                    ui.message_warning,
                    area.right(),
                );
            }
        }
        let cx = x + (before_w - skip) as u16;
        return Some(CursorShape::Bar(
            cx.min(area.right().saturating_sub(1)),
            area.y,
        ));
    }
    if let Some((sev, msg)) = &ed.status {
        let (prefix, style) = match sev {
            Severity::Info => ("", ui.message_info),
            Severity::Warning => ("warning: ", ui.message_warning),
            Severity::Error => ("error: ", ui.message_error),
        };
        let line = msg.lines().next().unwrap_or_default();
        s.print(
            area.x,
            area.y,
            &format!("{prefix}{line}"),
            style,
            area.right(),
        );
    } else if ed.mode != Mode::Insert
        && ed.picker.is_none()
        && let Some(d) = lsp::cursor_diagnostic(ed)
    {
        // When nothing else is said, the cursor line's diagnostic is.
        let style = ui.text.patch(diagnostic_fg(&ed.theme, d.severity));
        let line = d.message.lines().next().unwrap_or_default();
        let text = format!("{}: {line}", d.severity.label());
        s.print(area.x, area.y, &text, style, area.right());
    }
    // Pending state on the right: count, register, keys.
    let mut pending = String::new();
    if let Some(r) = ed.selected_register {
        pending.push_str(&format!("\"{r} "));
    }
    if let Some(c) = ed.count {
        pending.push_str(&c.to_string());
    }
    for k in &ed.pending {
        pending.push_str(&k.to_string());
        pending.push(' ');
    }
    if let Some(label) = ed.pending_char_label() {
        pending.push_str(&format!("{label}: _"));
    }
    if !pending.is_empty() {
        let w = pending.chars().count() as u16 + 1;
        if area.width > w {
            s.print(
                area.right() - w,
                area.y,
                &pending,
                ui.text_muted,
                area.right(),
            );
        }
    }
    None
}

/// Shows the continuations of a pending multi-key binding.
fn draw_which_key(ed: &Editor, s: &mut Surface, cmdline: Rect) {
    let Some(node) = ed.pending_node() else {
        return;
    };
    let Some(children) = node.children() else {
        return;
    };
    let ui = &ed.theme.ui;
    let rows: Vec<(String, String)> = children
        .iter()
        .map(|(k, v)| {
            let desc = match v {
                maestro_editor::keymap::KeyTrie::Command(c) => c.replace('_', " "),
                maestro_editor::keymap::KeyTrie::Node { name, .. } => format!("{name}…"),
            };
            (k.to_string(), desc)
        })
        .collect();
    let key_w = rows
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(1);
    let desc_w = rows
        .iter()
        .map(|(_, d)| d.chars().count())
        .max()
        .unwrap_or(1);
    let w = (key_w + desc_w + 5).min(s.width as usize) as u16;
    let h = (rows.len() as u16 + 2).min(cmdline.y);
    if h < 3 || w < 8 {
        return;
    }
    let x = s.width.saturating_sub(w + 1);
    let y = cmdline.y.saturating_sub(h);
    let area = Rect::new(x, y, w, h);
    draw_box(
        s,
        area,
        node.name(),
        ui.popup,
        ui.popup_border,
        ui.popup_title,
    );
    for (i, (k, d)) in rows.iter().take((h - 2) as usize).enumerate() {
        let ry = y + 1 + i as u16;
        s.print(x + 2, ry, k, ui.popup_title, area.right() - 1);
        s.print(x + 2 + key_w as u16 + 1, ry, d, ui.popup, area.right() - 1);
    }
}

pub(crate) fn draw_box(
    s: &mut Surface,
    area: Rect,
    title: &str,
    fill: Style,
    border: Style,
    title_style: Style,
) {
    // Too small to frame anything; skip rather than underflow.
    if area.width < 4 || area.height < 3 {
        return;
    }
    s.fill(area, fill);
    let (l, r, t, b) = (area.x, area.right() - 1, area.y, area.bottom() - 1);
    for x in l + 1..r {
        s.put(x, t, "─", border, r + 1);
        s.put(x, b, "─", border, r + 1);
    }
    for y in t + 1..b {
        s.put(l, y, "│", border, r + 1);
        s.put(r, y, "│", border, r + 1);
    }
    s.put(l, t, "╭", border, r + 1);
    s.put(r, t, "╮", border, r + 1);
    s.put(l, b, "╰", border, r + 1);
    s.put(r, b, "╯", border, r + 1);
    if !title.is_empty() {
        s.print(l + 2, t, &format!(" {title} "), title_style, r - 1);
    }
}

fn draw_picker(ed: &Editor, s: &mut Surface) -> Option<CursorShape> {
    let picker = ed.picker.as_ref()?;
    let ui = &ed.theme.ui;
    let preview = picker
        .preview
        .as_ref()
        .filter(|_| s.width >= maestro_editor::navigation::preview::MIN_WIDTH);
    let w = if preview.is_some() {
        (s.width as u32 * 9 / 10).clamp(20, 220) as u16
    } else {
        (s.width as u32 * 4 / 5).clamp(20, 120) as u16
    };
    let w = w.min(s.width);
    let h = (s.height as u32 * 3 / 5).clamp(6, 30) as u16;
    let h = h.min(s.height.saturating_sub(1));
    if h < 5 || w < 10 {
        return None;
    }
    let x = (s.width - w) / 2;
    let y = (s.height.saturating_sub(h)) / 3;
    let area = Rect::new(x, y, w, h);
    let title = format!(
        "{}  {}/{}{}",
        picker.title,
        picker.matches().len(),
        picker.total(),
        if picker.loading { "  loading" } else { "" }
    );
    draw_box(s, area, &title, ui.popup, ui.popup_border, ui.popup_title);
    // With a preview, the list takes the left half.
    let inner_r = match preview {
        Some(_) => x + 1 + (w - 2) / 2,
        None => area.right() - 1,
    };
    if let Some(pv) = preview {
        for yy in y + 1..area.bottom() - 1 {
            s.put(inner_r, yy, "│", ui.popup_border, inner_r + 1);
        }
        // Join the top border unless the title runs across the divider.
        if x as usize + 4 + title.chars().count() < inner_r as usize {
            s.put(inner_r, y, "┬", ui.popup_border, inner_r + 1);
        }
        s.put(
            inner_r,
            area.bottom() - 1,
            "┴",
            ui.popup_border,
            inner_r + 1,
        );
        let r = area.right() - 1;
        let parea = Rect::new(inner_r + 1, y + 1, r - inner_r - 1, h - 2);
        crate::preview::draw(ed, s, parea, pv);
    }
    let list_w = inner_r - x + 1;
    // Query line.
    let qx = s.print(x + 2, y + 1, "> ", ui.popup_title, inner_r);
    let end = s.print(qx, y + 1, &picker.query, ui.popup, inner_r);
    for xx in x + 1..inner_r {
        s.put(xx, y + 2, "─", ui.popup_border, inner_r);
    }
    let list_h = (h - 4) as usize;
    let sel = picker.selected;
    let scroll = if sel >= list_h { sel + 1 - list_h } else { 0 };
    if picker.matches().is_empty() {
        let msg = if picker.loading {
            "searching…"
        } else {
            "no matches"
        };
        s.print(x + 2, y + 3, msg, ui.popup_muted, inner_r);
    }
    for (row, m) in picker
        .matches()
        .iter()
        .skip(scroll)
        .take(list_h)
        .enumerate()
    {
        let item = &picker.items()[m.index];
        let ry = y + 3 + row as u16;
        let selected = scroll + row == sel;
        let (base, hl) = if selected {
            (
                ui.popup_selected,
                ui.popup_selected.patch(ui.popup_match_selected),
            )
        } else {
            (ui.popup, ui.popup.patch(ui.popup_match))
        };
        s.fill(Rect::new(x + 1, ry, list_w - 2, 1), base);
        let detail_w = item.detail.chars().count() as u16;
        let label_r = if detail_w > 0 && detail_w + 10 < list_w {
            inner_r - detail_w - 2
        } else {
            inner_r
        };
        let mut cx = x + 2;
        for (i, ch) in item.label.chars().enumerate() {
            let st = if m.positions.contains(&i) { hl } else { base };
            let used = s.put(cx, ry, &ch.to_string(), st, label_r);
            if used == 0 {
                break;
            }
            cx += used;
        }
        if label_r < inner_r {
            let dstyle = if selected { base } else { ui.popup_muted };
            s.print(inner_r - detail_w - 1, ry, &item.detail, dstyle, inner_r);
        }
    }
    Some(CursorShape::Bar(end.min(inner_r - 1), y + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_config::Config;
    use maestro_core::Document;
    use maestro_editor::git::{Sign, SignMap};

    /// Signs draw as symbols in the gutter's first cell, colored by role.
    #[test]
    fn git_signs_draw_symbols_in_the_sign_column() {
        let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 40, 10));
        let id = ed.add_document(Document::from_text("a\nb\nc\nd\n\n"));
        ed.switch_to(id);
        let (_, views, _) = split_screen(40, 10, &ed);
        ed.resize(views);
        ed.git
            .set_signs(id, SignMap::diff("a\nB\nc\nx\nd\n", "a\nb\nc\nd\n\n"));
        // Line 1 changed, "x" removed below line 2, line 4 added.
        assert_eq!(
            ed.git.signs(id).unwrap().iter().collect::<Vec<_>>(),
            vec![(1, Sign::Changed), (2, Sign::Removed), (4, Sign::Added)]
        );
        let mut s = Surface::new(40, 10, ed.theme.ui.text);
        render(&ed, &mut s);
        let top = ed.view().area.y;
        let cell = |line: u16| s.cell(0, top + line).unwrap().clone();
        assert_eq!(cell(0).symbol, " ");
        assert_eq!(cell(1).symbol, "~");
        assert_eq!(cell(1).style.fg, ed.theme.diff.git_changed.fg);
        assert_eq!(cell(1).style.bg, ed.theme.ui.gutter.bg);
        assert_eq!(cell(2).symbol, "-");
        assert_eq!(cell(2).style.fg, ed.theme.diff.git_removed.fg);
        assert_eq!(cell(3).symbol, " ");
        assert_eq!(cell(4).symbol, "+");
        assert_eq!(cell(4).style.fg, ed.theme.diff.git_add.fg);
        // Line numbers still follow the sign column.
        assert_eq!(s.cell(3, top + 1).unwrap().symbol, "2");
    }

    use maestro_editor::KeyEvent;
    use maestro_editor::lsp::{Diagnostic, HoverPopup};

    fn editor(text: &str) -> Editor {
        let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 60, 18));
        let id = ed.add_document(Document::from_text(text));
        ed.switch_to(id);
        ed
    }

    fn row(s: &Surface, y: u16) -> String {
        s.row(y).iter().map(|c| c.symbol.as_str()).collect()
    }

    #[test]
    fn diagnostics_draw_a_sign_a_range_and_a_message() {
        let mut ed = editor("fn main() {}\nlet x = bad;\n");
        let doc = ed.view().doc;
        ed.lsp.set_diagnostics(
            doc,
            vec![Diagnostic {
                from: 21,
                to: 24,
                severity: DiagnosticSeverity::Error,
                message: "cannot find value `bad`\nsecond line".into(),
                source: None,
                raw: Default::default(),
            }],
        );
        let mut s = Surface::new(60, 20, ed.theme.ui.text);
        render(&ed, &mut s);
        // Row 2 is the second text line (row 0 is the tab line).
        let sign = s.cell(0, 2).unwrap();
        assert_eq!(sign.symbol, "E");
        assert_eq!(sign.style.fg, ed.theme.diagnostic.error.fg);
        assert_eq!(sign.style.bg, ed.theme.ui.gutter.bg);
        assert_eq!(s.cell(0, 1).unwrap().symbol, " ", "no sign on line 1");
        // The range is tinted and underlined; its neighbors are not.
        let gutter = ed.gutter_width(ed.view());
        let b = s.cell(gutter + 8, 2).unwrap();
        assert_eq!(b.symbol, "b");
        assert!(b.style.underline);
        assert_eq!(b.style.bg, Some(ed.theme.diagnostic.error_bg));
        assert!(!s.cell(gutter + 7, 2).unwrap().style.underline);
        // The message shows in the command line only on the cursor's line.
        assert!(!row(&s, 19).contains("cannot find"));
        ed.handle_key(KeyEvent::char('j'));
        let mut s = Surface::new(60, 20, ed.theme.ui.text);
        render(&ed, &mut s);
        assert!(
            row(&s, 19).starts_with("error: cannot find value `bad`"),
            "{:?}",
            row(&s, 19)
        );
        assert_eq!(
            s.cell(0, 19).unwrap().style.fg,
            ed.theme.diagnostic.error.fg
        );
    }

    #[test]
    fn hover_popup_wraps_below_the_cursor() {
        let mut ed = editor("fn main() {}\n");
        let doc = ed.view().doc;
        ed.lsp.hover = Some(HoverPopup {
            doc,
            anchor: 3,
            lines: vec![
                "fn main()".into(),
                String::new(),
                "word ".repeat(30).trim_end().to_string(),
            ],
        });
        let mut s = Surface::new(60, 20, ed.theme.ui.text);
        render(&ed, &mut s);
        assert!(row(&s, 2).contains("hover"), "{:?}", row(&s, 2));
        assert!(row(&s, 3).contains("fn main()"));
        // 150 columns of text wrapped inside a 56-column box.
        assert!(row(&s, 5).contains("word word"));
        assert!(row(&s, 6).contains("word word"));
        assert!(row(&s, 5).trim_end().ends_with('│'));
    }

    #[test]
    fn hover_popup_never_panics_at_small_sizes() {
        for (w, h) in (1..=40u16).flat_map(|w| (1..=12u16).map(move |h| (w, h))) {
            let mut ed = Editor::new(Config::default(), Rect::new(0, 1, w, h.saturating_sub(2)));
            let id = ed.add_document(Document::from_text("fn main() {}\n"));
            ed.switch_to(id);
            ed.lsp.hover = Some(HoverPopup {
                doc: id,
                anchor: 3,
                lines: vec!["x".repeat(200), "short".into()],
            });
            let mut s = Surface::new(w, h, ed.theme.ui.text);
            render(&ed, &mut s);
        }
    }

    fn completion_items() -> maestro_editor::lsp::completion::Value {
        maestro_editor::lsp::completion::json!([
            {"label": "collections", "kind": 9, "detail": "std::collections", "sortText": "1"},
            {"label": "Color", "kind": 22, "sortText": "2"},
        ])
    }

    /// Label, kind as a word and detail on each row, under the word being
    /// completed, with the selected row's number in the title.
    #[test]
    fn completion_popup_lists_label_kind_and_detail_below_the_word() {
        use maestro_editor::lsp::completion;
        let mut ed = editor("let x = co\n");
        ed.mode = Mode::Insert;
        ed.set_selection(maestro_core::Selection::point(10));
        assert!(completion::show(&mut ed, &completion_items()));
        let mut s = Surface::new(60, 20, ed.theme.ui.text);
        render(&ed, &mut s);
        // Line 0 is screen row 1; the box starts on the row below.
        assert!(row(&s, 2).contains("1/2"), "{:?}", row(&s, 2));
        let first = row(&s, 3);
        assert!(first.contains("collections"), "{first:?}");
        assert!(first.contains("module"), "{first:?}");
        assert!(first.contains("std::collections"), "{first:?}");
        assert!(row(&s, 4).contains("Color") && row(&s, 4).contains("struct"));
        // The label column starts where `co` starts (gutter 5 + col 8).
        assert_eq!(s.cell(13, 3).unwrap().symbol, "c", "{first:?}");
        assert_eq!(s.cell(14, 3).unwrap().symbol, "o", "{first:?}");
        // The selected row uses the selected style, the other does not.
        let selected_bg = ed.theme.ui.popup_selected.bg;
        assert_eq!(s.cell(15, 3).unwrap().style.bg, selected_bg);
        assert_ne!(s.cell(15, 4).unwrap().style.bg, selected_bg);
    }

    #[test]
    fn completion_popup_flips_above_near_the_bottom() {
        use maestro_editor::lsp::completion;
        let text = "x\n".repeat(16) + "co\n";
        let mut ed = editor(&text);
        // No margin, so the last line stays on the screen's last text row.
        ed.config.editor.scrolloff = 0;
        ed.mode = Mode::Insert;
        let at = ed.doc().text().line_to_char(16) + 2;
        ed.set_selection(maestro_core::Selection::point(at));
        ed.ensure_cursor_visible();
        assert!(completion::show(&mut ed, &completion_items()));
        let mut s = Surface::new(60, 20, ed.theme.ui.text);
        render(&ed, &mut s);
        let line_y = (1 + 16 - ed.view().top_line) as u16;
        assert!(row(&s, line_y).contains("co"));
        assert!(
            row(&s, line_y - 1).trim_end().ends_with('╯'),
            "{:?}",
            (line_y, row(&s, line_y - 1), row(&s, line_y))
        );
        assert!(row(&s, line_y - 3).contains("collections"));
        assert!(!row(&s, line_y + 1).contains("collections"));
    }

    #[test]
    fn completion_popup_never_panics_at_small_sizes() {
        use maestro_editor::lsp::completion;
        for (w, h) in (1..=40u16).flat_map(|w| (1..=12u16).map(move |h| (w, h))) {
            let mut ed = Editor::new(Config::default(), Rect::new(0, 1, w, h.saturating_sub(2)));
            let id = ed.add_document(Document::from_text("let 日本 = co\n"));
            ed.switch_to(id);
            ed.mode = Mode::Insert;
            ed.set_selection(maestro_core::Selection::point(11));
            completion::show(&mut ed, &completion_items());
            let mut s = Surface::new(w, h, ed.theme.ui.text);
            render(&ed, &mut s);
        }
    }

    fn signature_reply(
        label: &str,
        params: &[(usize, usize)],
        active: usize,
    ) -> maestro_editor::lsp::completion::Value {
        let params: Vec<maestro_editor::lsp::completion::Value> = params
            .iter()
            .map(|(a, b)| maestro_editor::lsp::completion::json!({"label": [a, b]}))
            .collect();
        maestro_editor::lsp::completion::json!({
            "signatures": [{"label": label, "parameters": params}],
            "activeParameter": active,
        })
    }

    /// The signature sits in a box above the cursor line; the active
    /// parameter is bold and underlined, the others are not.
    #[test]
    fn signature_popup_marks_the_active_parameter_above_the_cursor() {
        use maestro_editor::lsp::signature;
        let mut ed = editor("fn f(a: u32, b: u32) {}\n\n\n\nf(1, \n");
        ed.mode = Mode::Insert;
        let at = ed.doc().text().line_to_char(4) + 5;
        ed.set_selection(maestro_core::Selection::point(at));
        let label = "fn f(a: u32, b: u32)";
        assert!(signature::show(
            &mut ed,
            &signature_reply(label, &[(5, 11), (13, 19)], 1)
        ));
        let mut s = Surface::new(60, 20, ed.theme.ui.text);
        render(&ed, &mut s);
        // Line 4 is screen row 5; the box is rows 2 to 4.
        assert!(row(&s, 2).contains("signature"), "{:?}", row(&s, 2));
        let text_row = row(&s, 3);
        // Columns, not bytes: the border glyph is three bytes wide.
        let byte = text_row.find(label).expect("label drawn");
        let x = text_row[..byte].chars().count() as u16;
        let style = |i: u16| s.cell(x + i, 3).unwrap().style;
        for i in 13..19 {
            assert!(style(i).underline && style(i).bold, "b: u32 at {i}");
        }
        for i in 5..11 {
            assert!(!style(i).underline, "a: u32 at {i}");
        }
        assert!(
            row(&s, 5).contains("f(1,"),
            "the cursor line is not covered"
        );
        // Outside insert mode nothing is drawn.
        ed.mode = Mode::Normal;
        let mut s = Surface::new(60, 20, ed.theme.ui.text);
        render(&ed, &mut s);
        assert!(!row(&s, 3).contains(label));
    }

    /// A label wider than the screen is cut so the active parameter stays
    /// in view, with `…` marking what is left out.
    #[test]
    fn signature_popup_keeps_a_late_active_parameter_visible() {
        use maestro_editor::lsp::signature;
        let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 30, 18));
        let id = ed.add_document(Document::from_text("\n\n\n\ncall(1, 2, 3\n"));
        ed.switch_to(id);
        ed.mode = Mode::Insert;
        let at = ed.doc().text().line_to_char(4) + 12;
        ed.set_selection(maestro_core::Selection::point(at));
        let label = "fn call(first: u32, second: u32, third_parameter: u32)";
        let third = label.find("third").unwrap();
        assert!(signature::show(
            &mut ed,
            &signature_reply(label, &[(8, 18), (20, 31), (third, third + 20)], 2)
        ));
        let mut s = Surface::new(30, 20, ed.theme.ui.text);
        render(&ed, &mut s);
        let text_row = row(&s, 3);
        assert!(text_row.contains('…'), "{text_row:?}");
        assert!(text_row.contains("third_parameter: u32"), "{text_row:?}");
        assert!(!text_row.contains("fn call"), "{text_row:?}");
    }

    #[test]
    fn signature_popup_never_panics_at_small_sizes() {
        use maestro_editor::lsp::signature;
        for (w, h) in (1..=40u16).flat_map(|w| (1..=12u16).map(move |h| (w, h))) {
            let mut ed = Editor::new(Config::default(), Rect::new(0, 1, w, h.saturating_sub(2)));
            let id = ed.add_document(Document::from_text("x\nf(日本, \n"));
            ed.switch_to(id);
            ed.mode = Mode::Insert;
            ed.set_selection(maestro_core::Selection::point(8));
            ed.ensure_cursor_visible();
            signature::show(
                &mut ed,
                &signature_reply("fn f(日本: u8, 😀: u8, c: u8)", &[(5, 10), (12, 18)], 1),
            );
            let mut s = Surface::new(w, h, ed.theme.ui.text);
            render(&ed, &mut s);
        }
    }

    #[test]
    fn wrap_breaks_at_spaces_and_inside_long_words() {
        assert_eq!(wrap("aa bb cc", 5), ["aa bb", "cc"]);
        assert_eq!(wrap("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(wrap("", 3), [""]);
    }

    #[test]
    fn errors_win_the_sign_cell_and_git_wins_over_hints() {
        use maestro_editor::git::SignMap;
        let mut ed = editor("a\nb\nc\n");
        let doc = ed.view().doc;
        // Lines 0 and 1 changed.
        ed.git
            .set_signs(doc, SignMap::diff("A\nB\nc\n", "a\nb\nc\n"));
        let diag = |from, severity| Diagnostic {
            from,
            to: from + 1,
            severity,
            message: "m".into(),
            source: None,
            raw: Default::default(),
        };
        ed.lsp.set_diagnostics(
            doc,
            vec![
                diag(0, DiagnosticSeverity::Error),
                diag(2, DiagnosticSeverity::Hint),
                diag(4, DiagnosticSeverity::Hint),
            ],
        );
        let mut s = Surface::new(60, 20, ed.theme.ui.text);
        render(&ed, &mut s);
        let top = ed.view().area.y;
        assert_eq!(s.cell(0, top).unwrap().symbol, "E", "error over a git sign");
        assert_eq!(
            s.cell(0, top + 1).unwrap().symbol,
            "~",
            "git sign over a hint"
        );
        assert_eq!(s.cell(0, top + 2).unwrap().symbol, "H", "hint alone shows");
    }
}
