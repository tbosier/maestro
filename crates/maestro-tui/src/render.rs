//! Draws editor state into a [`Surface`]. Reads the editor; never changes it.

use maestro_config::{LineNumbers, Style, Theme};
use maestro_core::text::{grapheme_width, is_line_ending_str, next_grapheme_boundary};
use maestro_editor::editor::Buffer;
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
    let mut cursor = CursorShape::Hidden;
    if ed.mode == Mode::Insert && ed.prompt.is_none() && ed.picker.is_none() {
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
    if h < 3 {
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

fn draw_box(
    s: &mut Surface,
    area: Rect,
    title: &str,
    fill: Style,
    border: Style,
    title_style: Style,
) {
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
