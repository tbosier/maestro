//! Draws the agent panel and the review screen over the editor area.
//! Reads the editor; never changes it.

use maestro_config::{Rgb, Style, Theme};
use maestro_editor::Editor;
use maestro_editor::Rect;
use maestro_editor::agents::panel::{self, ConfirmAction, DetailView, Overlay, PanelLayout};
use maestro_editor::agents::review::{self, Row, RowLine};
use maestro_editor::agents::{
    Agent, DiffLineKind, FileChange, ProviderKind, Runtime, Screen, Status, TermColor,
    TranscriptEntry,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::render::{CursorShape, draw_box, wrap};
use crate::surface::Surface;

/// The theme color for a status. The name is always drawn with it.
pub fn status_style(theme: &Theme, s: Status) -> Style {
    let a = &theme.agent;
    match s {
        Status::Starting => a.starting,
        Status::Working => a.working,
        Status::Planning => a.planning,
        Status::Editing => a.editing,
        Status::AwaitingApproval => a.awaiting_approval,
        Status::Running | Status::RunningTests => a.running,
        Status::ReadyForReview => a.ready,
        Status::Blocked => a.blocked,
        Status::Failed => a.failed,
        Status::Rejected => a.rejected,
        Status::Integrated => a.completed,
    }
}

pub(crate) fn draw(ed: &Editor, s: &mut Surface) -> CursorShape {
    let area = ed.area;
    if area.width < 20 || area.height < 6 {
        s.fill(area, ed.theme.ui.text);
        s.print(
            area.x,
            area.y,
            "agent panel: the window is too small",
            ed.theme.ui.message_warning,
            area.right(),
        );
        return CursorShape::Hidden;
    }
    let l = panel::layout(area);
    let ui = &ed.theme.ui;
    s.fill(area, ui.text);
    draw_header(ed, s, &l);
    for y in l.divider.y..l.divider.bottom() {
        s.put(l.divider.x, y, "│", ui.border, l.divider.right());
    }
    let mut cursor = CursorShape::Hidden;
    if ed.agents.panel.review.is_some() {
        draw_review(ed, s, &l);
    } else {
        draw_list(ed, s, l.list);
        if let Some(a) = ed.agents.selected() {
            draw_summary(ed, s, l.summary, a);
            if let Some(c) = draw_content(ed, s, l.content, a) {
                cursor = c;
            }
        } else {
            let msg = "No agents. Press n to start a task: Maestro makes a worktree on a new branch and runs the provider you pick in it.";
            draw_wrapped(
                s,
                l.summary.x + 1,
                l.summary.y,
                l.summary.width.saturating_sub(2),
                4,
                msg,
                ui.text_muted,
            );
        }
    }
    draw_hints(ed, s, l.hints);
    if let Some(c) = draw_overlay(ed, s, area) {
        cursor = c;
    }
    cursor
}

fn draw_header(ed: &Editor, s: &mut Surface, l: &PanelLayout) {
    let ui = &ed.theme.ui;
    let h = l.header;
    s.fill(h, ui.statusline);
    let running = ed.agents.running().len();
    let title = match &ed.agents.panel.review {
        Some(v) => format!(
            " review: {} ",
            ed.agents
                .get(v.agent)
                .map(|a| a.name.as_str())
                .unwrap_or("?")
        ),
        None => format!(
            " agents: {} total, {running} running ",
            ed.agents.agents.len()
        ),
    };
    let x = s.print(h.x, h.y, &title, ui.statusline.bold(), h.right());
    // Who has the keys, always on screen.
    let (badge, style) = match ed.agents.panel.terminal_focus {
        Some(id) => (
            format!(
                " KEYS: {} TERMINAL (C-\\ takes them back) ",
                ed.agents
                    .get(id)
                    .map(|a| a.name.as_str())
                    .unwrap_or("agent")
            ),
            ui.mode_insert,
        ),
        None => (" KEYS: PANEL ".to_string(), ui.mode_normal),
    };
    let w = badge.chars().count() as u16;
    let bx = h.right().saturating_sub(w).max(x + 1);
    s.print(bx, h.y, &badge, style, h.right());
}

fn draw_hints(ed: &Editor, s: &mut Surface, area: Rect) {
    if area.height == 0 {
        return;
    }
    let ui = &ed.theme.ui;
    s.fill(area, ui.text);
    let mut x = area.x + 1;
    for (k, d) in panel::hints(ed) {
        let need = (k.chars().count() + d.chars().count() + 3) as u16;
        if x + need > area.right() {
            break;
        }
        x = s.print(x, area.y, k, ui.text.bold(), area.right());
        x = s.print(x + 1, area.y, d, ui.text_muted, area.right());
        x += 2;
    }
}

/// One-line facts for the list: `#1 acp  3 files +12 -4  ok 40`.
fn list_detail(a: &Agent) -> String {
    let mut d = format!(
        "#{} {}",
        a.group,
        maestro_editor::agents::kind_label(a.kind())
    );
    if a.archived {
        d.push_str("  archived");
    } else if a.review.computed.is_some() {
        let (f, add, rem) = a.review.totals();
        d.push_str(&format!("  {f} files +{add} -{rem}"));
    }
    if let Some(t) = a.latest_test() {
        d.push_str(&format!("  {}", t.short()));
    }
    d
}

fn draw_list(ed: &Editor, s: &mut Surface, area: Rect) {
    let ui = &ed.theme.ui;
    let sel = ed.agents.panel.selected;
    let per = 2usize;
    let visible = (area.height as usize / per).max(1);
    let first = if sel >= visible { sel + 1 - visible } else { 0 };
    for (i, a) in ed
        .agents
        .agents
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
    {
        let y = area.y + ((i - first) * per) as u16;
        let selected = i == sel;
        let bg = if selected { ui.popup } else { ui.text };
        s.fill(Rect::new(area.x, y, area.width, per as u16), bg);
        if selected {
            s.put(
                area.x,
                y,
                "▌",
                ui.border_active
                    .patch(Style::bg(bg.bg.unwrap_or(ed.theme.background))),
                area.x + 1,
            );
            s.put(
                area.x,
                y + 1,
                "▌",
                ui.border_active
                    .patch(Style::bg(bg.bg.unwrap_or(ed.theme.background))),
                area.x + 1,
            );
        }
        let x = s.print(area.x + 2, y, &a.name, bg.bold(), area.right());
        let status = a.status();
        s.print(
            x + 2,
            y,
            status.label(),
            bg.patch(status_style(&ed.theme, status)),
            area.right(),
        );
        let muted = if selected {
            ui.popup_muted
        } else {
            ui.text_muted
        };
        s.print(area.x + 2, y + 1, &list_detail(a), muted, area.right());
    }
}

fn draw_summary(ed: &Editor, s: &mut Surface, area: Rect, a: &Agent) {
    let ui = &ed.theme.ui;
    let r = area.right();
    let x0 = area.x + 1;
    let mut y = area.y;
    // Row 1: name, status, provider.
    let mut x = s.print(x0, y, &a.name, ui.text.bold(), r);
    let st = a.status();
    x = s.print(
        x + 2,
        y,
        st.label(),
        ui.text.patch(status_style(&ed.theme, st)),
        r,
    );
    let mut tail = format!(
        "  {} ({})",
        a.provider.name,
        maestro_editor::agents::kind_label(a.kind())
    );
    if let Some(e) = &a.exited {
        tail.push_str(&format!("  process {e}"));
    }
    s.print(x, y, &tail, ui.text_muted, r);
    y += 1;
    if y >= area.bottom() {
        return;
    }
    // Row 2: what the status can be based on.
    let note = match a.kind() {
        ProviderKind::Acp => {
            "ACP: status comes from the agent's plan, tool, permission and turn messages"
        }
        ProviderKind::Terminal => {
            "terminal only: no structured status; Maestro sees the screen, the exit code and the files"
        }
    };
    s.print(x0, y, note, ui.text_muted, r);
    y += 1;
    if y >= area.bottom() {
        return;
    }
    // Row 3: worktree.
    let wt = match &a.worktree {
        Some(w) if a.archived => format!("worktree archived; branch {} kept", w.branch),
        Some(w) => {
            // The branch first; the path gives way from the left.
            let head = format!("{} from {}  ", w.branch, &w.base[..w.base.len().min(8)]);
            let room = (r.saturating_sub(x0) as usize).saturating_sub(head.chars().count());
            format!("{head}{}", tail_of(&w.path.display().to_string(), room))
        }
        None => "worktree not created yet".to_string(),
    };
    s.print(x0, y, &wt, ui.text_muted, r);
    y += 1;
    if y >= area.bottom() {
        return;
    }
    // Row 4: task and the view.
    let view = format!("  [{} view, v]", ed.agents.panel.view.label());
    let vw = view.chars().count() as u16;
    let task_r = r.saturating_sub(vw).max(x0);
    let first = a.task.lines().next().unwrap_or_default();
    let x = s.print(x0, y, "task: ", ui.text_muted, task_r);
    s.print(x, y, first, ui.text, task_r);
    s.print(task_r, y, &view, ui.text_muted, r);
}

/// The end of `text` in at most `width` columns, with `…` when cut.
fn tail_of(text: &str, width: usize) -> String {
    let n = text.chars().count();
    if n <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let kept: String = text.chars().skip(n - (width - 1)).collect();
    format!("…{kept}")
}

fn draw_content(ed: &Editor, s: &mut Surface, area: Rect, a: &Agent) -> Option<CursorShape> {
    let ui = &ed.theme.ui;
    match ed.agents.panel.view {
        DetailView::Live => match &a.runtime {
            Runtime::Terminal(t) => {
                let screen = t.snapshot();
                draw_screen(&ed.theme, s, area, &screen);
                if screen.scrolled {
                    let msg = " scrolled back: pagedown returns ";
                    s.print(
                        area.right().saturating_sub(msg.len() as u16),
                        area.y,
                        msg,
                        ui.mode_command,
                        area.right(),
                    );
                }
                if ed.agents.panel.terminal_focus == Some(a.id)
                    && let Some((row, col)) = screen.cursor
                    && row < area.height
                    && col < area.width
                {
                    return Some(CursorShape::Bar(area.x + col, area.y + row));
                }
                None
            }
            _ => {
                let lines = transcript_lines(ed, a, area.width.saturating_sub(2) as usize);
                draw_lines(s, area, &lines, ed.agents.panel.scroll);
                None
            }
        },
        DetailView::History => {
            let width = area.width.saturating_sub(2) as usize;
            let mut lines = Vec::new();
            for h in &a.history {
                let secs =
                    h.at.duration_since(a.created)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                let text = format!("+{:02}:{:02}  {}", secs / 60, secs % 60, h.text);
                for w in wrap(&text, width) {
                    lines.push((w, ui.text));
                }
            }
            if lines.is_empty() {
                lines.push(("no events yet".into(), ui.text_muted));
            }
            draw_lines(s, area, &lines, ed.agents.panel.scroll);
            None
        }
        DetailView::Tests => {
            let width = area.width.saturating_sub(2) as usize;
            let mut lines = Vec::new();
            match a.latest_test() {
                None => lines.push((
                    "no test runs yet; t runs the verification command in the worktree".into(),
                    ui.text_muted,
                )),
                Some(run) => {
                    let head = match &run.finished {
                        Some(f) => format!(
                            "{}: {} in {:.1}s",
                            run.command,
                            run.describe(),
                            f.duration.as_secs_f64()
                        ),
                        None => format!(
                            "{}: running for {}s",
                            run.command,
                            run.started.elapsed().as_secs()
                        ),
                    };
                    let style = match run.passed() {
                        Some(true) => ui.text.patch(ed.theme.agent.ready),
                        Some(false) => ui.text.patch(ed.theme.agent.failed),
                        None => ui.text.patch(ed.theme.agent.running),
                    };
                    for w in wrap(&head, width) {
                        lines.push((w, style.bold()));
                    }
                    if let Some(sum) = run.finished.as_ref().and_then(|f| f.summary.as_ref()) {
                        for f in &sum.failures {
                            lines.push((
                                format!("failed: {f}"),
                                ui.text.patch(ed.theme.agent.failed),
                            ));
                        }
                    }
                    if run.dropped > 0 {
                        lines.push((
                            format!("({} earlier lines dropped)", run.dropped),
                            ui.text_muted,
                        ));
                    }
                    for l in &run.output {
                        for w in wrap(l, width) {
                            lines.push((w, ui.text));
                        }
                    }
                }
            }
            draw_lines(s, area, &lines, ed.agents.panel.scroll);
            None
        }
    }
}

/// Lines for an ACP agent's transcript, wrapped to `width`.
fn transcript_lines(ed: &Editor, a: &Agent, width: usize) -> Vec<(String, Style)> {
    let ui = &ed.theme.ui;
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut push = |prefix: &str, text: &str, style: Style| {
        let mut first = true;
        for raw in text.split('\n') {
            let line = if first {
                format!("{prefix}{raw}")
            } else {
                format!("  {raw}")
            };
            first = false;
            for w in wrap(&line, width) {
                out.push((w, style));
            }
        }
    };
    if a.kind() == ProviderKind::Terminal {
        let msg = match (&a.runtime, a.status()) {
            (_, Status::Starting) => "creating the worktree…",
            _ => "the terminal is not running",
        };
        push("", msg, ui.text_muted);
    }
    for e in &a.transcript {
        match e {
            TranscriptEntry::User(t) => push("you: ", t, ui.text.bold()),
            TranscriptEntry::Agent(t) => push("agent: ", t, ui.text),
            TranscriptEntry::Thought(t) => push("thinking: ", t, ui.text_muted),
            TranscriptEntry::Tool {
                title,
                kind,
                status,
                locations,
                ..
            } => {
                let paths: Vec<String> = locations
                    .iter()
                    .map(|p| {
                        a.worktree
                            .as_ref()
                            .and_then(|w| p.strip_prefix(&w.path).ok())
                            .unwrap_or(p)
                            .display()
                            .to_string()
                    })
                    .collect();
                let text = format!("[{status}] {title} ({kind}) {}", paths.join(", "));
                push(
                    "tool ",
                    text.trim_end(),
                    ui.text.patch(ed.theme.agent.running),
                );
            }
            TranscriptEntry::Plan(entries) => {
                push("plan:", "", ui.text.patch(ed.theme.agent.planning));
                for (text, status) in entries {
                    let mark = match status.as_str() {
                        "completed" => "[x]",
                        "in_progress" => "[~]",
                        _ => "[ ]",
                    };
                    push(&format!("  {mark} "), text, ui.text);
                }
            }
            TranscriptEntry::System(t) => push("", t, ui.text_muted),
        }
    }
    for e in &a.errors {
        push("error: ", e, ui.message_error);
    }
    if let Some(p) = &a.permission {
        let warn = ui.text.patch(ed.theme.agent.awaiting_approval).bold();
        push("permission requested: ", &p.tool_title, warn);
        let opts: Vec<String> = p
            .options
            .iter()
            .map(|o| format!("{} ({})", o.label, o.kind.replace('_', " ")))
            .collect();
        push("choices: ", &opts.join(", "), warn);
        push("", "p answers it", ui.text_muted);
    }
    if out.is_empty() {
        let msg = match a.status() {
            Status::Starting => "starting…",
            _ => "nothing yet",
        };
        out.push((msg.to_string(), ui.text_muted));
    }
    out
}

/// Draws the last lines that fit, `scroll` lines up from the bottom.
fn draw_lines(s: &mut Surface, area: Rect, lines: &[(String, Style)], scroll: usize) {
    let h = area.height as usize;
    let end = lines
        .len()
        .saturating_sub(scroll.min(lines.len().saturating_sub(h.min(lines.len()))));
    let start = end.saturating_sub(h);
    for (i, (text, style)) in lines[start..end].iter().enumerate() {
        s.print(area.x + 1, area.y + i as u16, text, *style, area.right());
    }
}

fn draw_wrapped(s: &mut Surface, x: u16, y: u16, w: u16, max_rows: u16, text: &str, style: Style) {
    for (i, line) in wrap(text, w as usize)
        .iter()
        .take(max_rows as usize)
        .enumerate()
    {
        s.print(x, y + i as u16, line, style, x + w);
    }
}

fn term_rgb(theme: &Theme, c: TermColor, default: Rgb) -> Rgb {
    match c {
        TermColor::Default => default,
        TermColor::Indexed(i) => theme.term_color(i),
        TermColor::Rgb(r, g, b) => Rgb(r, g, b),
    }
}

/// Draws a terminal screen with its colors: default colors are the
/// theme's text colors, the 16 base colors the theme's terminal colors,
/// and the rest as the program set them.
pub(crate) fn draw_screen(theme: &Theme, s: &mut Surface, area: Rect, screen: &Screen) {
    let dfg = theme.ui.text.fg.unwrap_or(Rgb(255, 255, 255));
    let dbg = theme.background;
    for row in 0..screen.rows.min(area.height) {
        let mut col = 0;
        while col < screen.cols.min(area.width) {
            let Some(cell) = screen.cell(row, col) else {
                break;
            };
            let mut fg = term_rgb(theme, cell.fg, dfg);
            let mut bg = term_rgb(theme, cell.bg, dbg);
            if cell.inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cell.dim {
                fg = fg.blend(bg, 0.4);
            }
            let mut style = Style::on(fg, bg);
            style.bold = cell.bold;
            style.italic = cell.italic;
            style.underline = cell.underline;
            let x = area.x + col;
            if cell.text.is_empty() {
                // The right half of a wide character drawn already.
                col += 1;
                continue;
            }
            if s.put(x, area.y + row, &cell.text, style, area.right()) == 0 {
                s.put(x, area.y + row, " ", style, area.right());
            }
            col += 1;
        }
    }
}

// ----- review --------------------------------------------------------------

fn kind_letter(c: &FileChange) -> &'static str {
    match c {
        FileChange::Added => "A",
        FileChange::Deleted => "D",
        FileChange::Modified => "M",
        FileChange::Renamed { .. } => "R",
        FileChange::Copied { .. } => "C",
    }
}

fn draw_review(ed: &Editor, s: &mut Surface, l: &PanelLayout) {
    let ui = &ed.theme.ui;
    let Some(v) = &ed.agents.panel.review else {
        return;
    };
    let Some(a) = ed.agents.get(v.agent) else {
        return;
    };
    // File list.
    let list = l.list;
    let files = &a.review.files;
    let visible = list.height as usize;
    let first = if v.file >= visible {
        v.file + 1 - visible
    } else {
        0
    };
    for (i, f) in files.iter().enumerate().skip(first).take(visible) {
        let y = list.y + (i - first) as u16;
        let selected = i == v.file;
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
        let (add, rem) = f.counts();
        let counts = format!(" +{add} -{rem}");
        let cw = counts.chars().count() as u16;
        let name_r = list.right().saturating_sub(cw);
        let x = s.print(list.x + 2, y, kind_letter(&f.change), bg.bold(), name_r);
        let path = f
            .path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        s.print(x + 1, y, &path, bg, name_r);
        let x = s.print(
            name_r,
            y,
            &format!(" +{add}"),
            bg.patch(ed.theme.diff.git_add),
            list.right(),
        );
        s.print(
            x,
            y,
            &format!(" -{rem}"),
            bg.patch(ed.theme.diff.git_removed),
            list.right(),
        );
    }
    if files.is_empty() {
        let msg = if a.review.running || a.review.computed.is_none() {
            "computing the diff…"
        } else {
            "no changes since the base commit"
        };
        s.print(list.x + 2, list.y, msg, ui.text_muted, list.right());
    }
    // Summary.
    let sm = l.summary;
    let r = sm.right();
    let (n, add, rem) = a.review.totals();
    let st = a.status();
    let mut x = s.print(sm.x + 1, sm.y, &a.name, ui.text.bold(), r);
    x = s.print(
        x + 2,
        sm.y,
        st.label(),
        ui.text.patch(status_style(&ed.theme, st)),
        r,
    );
    s.print(
        x + 2,
        sm.y,
        &format!("{n} files +{add} -{rem}"),
        ui.text_muted,
        r,
    );
    if sm.height > 1 {
        let line = match files.get(v.file) {
            Some(f) => format!(
                "{}  {}  [{} layout, s]",
                f.path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                review::change_label(f),
                if v.side_by_side {
                    "side-by-side"
                } else {
                    "unified"
                }
            ),
            None => String::new(),
        };
        s.print(sm.x + 1, sm.y + 1, &line, ui.text, r);
    }
    if sm.height > 2 {
        let unsent = a.comments.iter().filter(|c| !c.sent).count();
        let state = if a.review.running {
            "refreshing".to_string()
        } else if let Some(t) = a.review.computed {
            format!("computed {}s ago (r refreshes)", t.elapsed().as_secs())
        } else {
            "not computed yet".to_string()
        };
        let text = format!("{unsent} unsent comment(s)  diff {state}");
        s.print(sm.x + 1, sm.y + 2, &text, ui.text_muted, r);
    }
    if sm.height > 3
        && let Some(e) = &a.review.error
    {
        s.print(
            sm.x + 1,
            sm.y + 3,
            &format!("diff failed: {e}"),
            ui.message_error,
            r,
        );
    }
    // Diff.
    let c = l.content;
    let rows = review::current_rows(ed);
    for (i, row) in rows.iter().enumerate().skip(v.top).take(c.height as usize) {
        let y = c.y + (i - v.top) as u16;
        draw_row(ed, s, Rect::new(c.x, y, c.width, 1), row);
        if i == v.cursor {
            s.put(c.x, y, "▶", ui.border_active, c.x + 1);
        }
    }
}

fn draw_row(ed: &Editor, s: &mut Surface, area: Rect, row: &Row) {
    let ui = &ed.theme.ui;
    let d = &ed.theme.diff;
    match row {
        Row::Hunk { header, .. } => {
            s.print(area.x + 2, area.y, header, d.hunk, area.right());
        }
        Row::Note(t) => {
            s.print(area.x + 2, area.y, t, ui.text_muted, area.right());
        }
        Row::Comment { text, sent } => {
            let t = if *sent {
                format!("comment (sent): {text}")
            } else {
                format!("comment: {text}")
            };
            s.print(area.x + 4, area.y, &t, d.comment, area.right());
        }
        Row::Line(l) => {
            let nums = format!(
                "{:>4} {:>4} ",
                l.old.map(|n| n.to_string()).unwrap_or_default(),
                l.new.map(|n| n.to_string()).unwrap_or_default()
            );
            let x = s.print(area.x + 1, area.y, &nums, ui.text_muted, area.right());
            draw_side(
                ed,
                s,
                Rect::new(x, area.y, area.right().saturating_sub(x), 1),
                l,
            );
        }
        Row::Pair(left, right) => {
            let half = area.width.saturating_sub(1) / 2;
            let lx = area.x + 1;
            let rx = lx + half + 1;
            for (side, x, w) in [
                (left, lx, half),
                (right, rx, area.right().saturating_sub(rx)),
            ] {
                if let Some(l) = side {
                    let n = l.new.or(l.old).map(|n| n.to_string()).unwrap_or_default();
                    let nx = s.print(x, area.y, &format!("{n:>4} "), ui.text_muted, x + w);
                    draw_side(
                        ed,
                        s,
                        Rect::new(nx, area.y, (x + w).saturating_sub(nx), 1),
                        l,
                    );
                }
            }
            s.put(rx - 1, area.y, "│", ui.border, rx);
        }
    }
}

/// A sign and a line of text on its add or delete tint, with intraline
/// marks on the stronger tint.
fn draw_side(ed: &Editor, s: &mut Surface, area: Rect, l: &RowLine) {
    let ui = &ed.theme.ui;
    let d = &ed.theme.diff;
    if area.width < 2 {
        return;
    }
    let (sign, sign_style, bg, mark_bg) = match l.kind {
        DiffLineKind::Added => ("+", d.git_add, Some(d.add_bg), d.add_text_bg),
        DiffLineKind::Removed => ("-", d.git_removed, Some(d.delete_bg), d.delete_text_bg),
        DiffLineKind::Context => (" ", ui.text, None, ed.theme.background),
    };
    s.put(
        area.x,
        area.y,
        sign,
        ui.text.patch(sign_style),
        area.right(),
    );
    let tx = area.x + 1;
    let base = match bg {
        Some(b) => ui.text.patch(Style::bg(b)),
        None => ui.text,
    };
    if bg.is_some() {
        s.fill(
            Rect::new(tx, area.y, area.right().saturating_sub(tx), 1),
            base,
        );
    }
    let mut x = tx;
    let mut col = 0usize;
    for (at, g) in l.text.grapheme_indices(true) {
        let marked = l.marks.iter().any(|m| at >= m.start && at < m.end);
        let style = if marked {
            base.patch(Style::bg(mark_bg))
        } else {
            base
        };
        if g == "\t" {
            let n = 4 - col % 4;
            for _ in 0..n {
                if s.put(x, area.y, " ", style, area.right()) == 0 {
                    return;
                }
                x += 1;
            }
            col += n;
            continue;
        }
        let used = s.put(x, area.y, g, style, area.right());
        if used == 0 {
            return;
        }
        x += used;
        col += used as usize;
    }
}

// ----- overlays --------------------------------------------------------------

fn draw_overlay(ed: &Editor, s: &mut Surface, area: Rect) -> Option<CursorShape> {
    let ui = &ed.theme.ui;
    let overlay = ed.agents.panel.overlay.as_ref()?;
    let w = area.width.saturating_sub(4).min(90);
    let inner = w.saturating_sub(4) as usize;
    let x = area.x + (area.width - w) / 2;
    match overlay {
        Overlay::Input { label, text, .. } => {
            let shown = text.replace('\n', "↵");
            let lines = wrap(&shown, inner.max(1));
            let h = (lines.len() as u16 + 2)
                .min(area.height.saturating_sub(2))
                .max(3);
            let y = area.y + (area.height.saturating_sub(h)) / 3;
            let b = Rect::new(x, y, w, h);
            draw_box(s, b, label, ui.popup, ui.popup_border, ui.popup_title);
            let rows = (h - 2) as usize;
            let skip = lines.len().saturating_sub(rows);
            let mut end = (x + 2, y + 1);
            for (i, line) in lines.iter().skip(skip).enumerate() {
                let ex = s.print(x + 2, y + 1 + i as u16, line, ui.popup, b.right() - 1);
                end = (ex, y + 1 + i as u16);
            }
            Some(CursorShape::Bar(end.0.min(b.right() - 2), end.1))
        }
        Overlay::Choice {
            title,
            items,
            selected,
            ..
        } => {
            let h = (items.len() as u16 + 2)
                .min(area.height.saturating_sub(2))
                .max(3);
            let y = area.y + (area.height.saturating_sub(h)) / 3;
            let b = Rect::new(x, y, w, h);
            draw_box(s, b, title, ui.popup, ui.popup_border, ui.popup_title);
            let rows = (h - 2) as usize;
            let first = if *selected >= rows {
                selected + 1 - rows
            } else {
                0
            };
            for (i, (label, detail)) in items.iter().enumerate().skip(first).take(rows) {
                let ry = y + 1 + (i - first) as u16;
                let style = if i == *selected {
                    ui.popup_selected
                } else {
                    ui.popup
                };
                s.fill(Rect::new(x + 1, ry, w - 2, 1), style);
                let lx = s.print(x + 2, ry, label, style, b.right() - 1);
                let dstyle = if i == *selected {
                    style
                } else {
                    ui.popup_muted
                };
                s.print(lx + 2, ry, detail, dstyle, b.right() - 1);
            }
            None
        }
        Overlay::Confirm(c) => {
            let mut lines: Vec<(String, Style)> = Vec::new();
            for l in &c.lines {
                for wl in wrap(l, inner.max(1)) {
                    lines.push((wl, ui.popup));
                }
            }
            let forced = matches!(c.action, ConfirmAction::ForceArchive(_));
            let h = (lines.len() as u16 + 4)
                .min(area.height.saturating_sub(1))
                .max(4);
            let y = area.y + (area.height.saturating_sub(h)) / 3;
            let b = Rect::new(x, y, w, h);
            let title = if forced {
                format!("{}: this deletes files", c.title)
            } else {
                c.title.clone()
            };
            draw_box(s, b, &title, ui.popup, ui.popup_border, ui.popup_title);
            let rows = (h - 4) as usize;
            for (i, (text, style)) in lines.iter().take(rows).enumerate() {
                s.print(x + 2, y + 1 + i as u16, text, *style, b.right() - 1);
            }
            let ky = b.bottom() - 2;
            let mut kx = x + 2;
            for (k, d) in &c.keys {
                kx = s.print(kx, ky, k, ui.popup_title, b.right() - 1);
                kx = s.print(kx + 1, ky, d, ui.popup, b.right() - 1);
                kx += 3;
            }
            None
        }
    }
}

#[cfg(test)]
#[path = "agents_tests.rs"]
mod tests;
