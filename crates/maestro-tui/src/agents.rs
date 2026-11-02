//! Draws the agent panel and the review screen over the editor area.
//! Reads the editor; never changes it.

use maestro_config::{Rgb, Style, Theme};
use maestro_editor::Editor;
use maestro_editor::Rect;
use maestro_editor::agents::panel::{self, ConfirmAction, DetailView, Overlay, PanelLayout};
use maestro_editor::agents::review::{self, Row, RowLine};
use maestro_editor::agents::{
    Agent, DiffLineKind, FileChange, Origin, ProviderKind, Runtime, Screen, Status, TermColor,
    TranscriptEntry,
};
use maestro_editor::agents::{compose, conflicts, contract::Contract, marks};
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
        Status::Idle => a.working,
        Status::Stopped => a.starting,
        Status::ReadyForReview => a.ready,
        Status::Blocked => a.blocked,
        Status::Failed => a.failed,
        Status::Rejected => a.rejected,
        Status::Completed | Status::Integrated => a.completed,
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
    let l = panel::agent_layout(area);
    let ui = &ed.theme.ui;
    s.fill(area, ui.text);
    draw_header(ed, s, &l);
    for y in l.divider.y..l.divider.bottom() {
        s.put(l.divider.x, y, "│", ui.border, l.divider.right());
    }
    let mut cursor = CursorShape::Hidden;
    if ed.agents.panel.compose.is_some() {
        draw_compose(ed, s, &l);
    } else if ed.agents.panel.review.is_some() {
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
        _ if ed.agents.panel.compose.is_some() => format!(
            " compose: task #{} ",
            ed.agents.panel.compose.as_ref().map_or(0, |c| c.group)
        ),
        Some(v) => format!(
            " review: {} ",
            ed.agents
                .get(v.agent)
                .map(|a| a.name.as_str())
                .unwrap_or("?")
        ),
        None if ed.agents.persist.loading => format!(
            " agents: {} total, {running} running; reading saved records… ",
            ed.agents.agents.len()
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

/// What the list shows after the name: the status, or for a found
/// worktree that it has none.
fn list_status(a: &Agent) -> &'static str {
    match a.origin {
        Origin::Found => "found worktree",
        _ => a.status().label(),
    }
}

/// The marker next to the status of an agent with no process from an
/// earlier session (the summary says it in full).
pub const EARLIER: &str = "not running (from an earlier session)";

/// One-line facts for the list: `#1 acp  3 files +12 -4  ok 40`.
/// The list's second line. What needs the reviewer's attention comes
/// first, so a narrow list does not cut it off: files outside the task
/// contract, then overlaps and conflicts with other agents.
fn list_detail(ed: &Editor, a: &Agent) -> String {
    let mut d = String::new();
    let outside = a.out_of_scope().len();
    if outside > 0 && !a.archived {
        d.push_str(&format!("! {outside} outside contract  "));
    }
    if let Some(c) = conflicts::brief(ed, a.id) {
        d.push_str(&format!("{c}  "));
    }
    d.push_str(&list_facts(a));
    d
}

fn list_facts(a: &Agent) -> String {
    let mut d = match a.origin {
        Origin::Found => "no record".to_string(),
        Origin::Composed => format!("#{} composed", a.group),
        _ if a.from_earlier_session() => format!(
            "not running, earlier session  #{} {}",
            a.group,
            maestro_editor::agents::kind_label(a.kind())
        ),
        _ => format!(
            "#{} {}",
            a.group,
            maestro_editor::agents::kind_label(a.kind())
        ),
    };
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
            list_status(a),
            bg.patch(status_style(&ed.theme, status)),
            area.right(),
        );
        let muted = if selected {
            ui.popup_muted
        } else {
            ui.text_muted
        };
        s.print(area.x + 2, y + 1, &list_detail(ed, a), muted, area.right());
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
        list_status(a),
        ui.text.patch(status_style(&ed.theme, st)),
        r,
    );
    if a.origin != Origin::Found && a.from_earlier_session() {
        x = s.print(x + 2, y, EARLIER, ui.text.bold(), r);
    }
    let mut tail = match a.origin {
        Origin::Found => "  provider unknown".to_string(),
        Origin::Composed => format!(
            "  composed from {} hunk(s)",
            a.composition.as_ref().map_or(0, |c| c.sources.len())
        ),
        _ => format!(
            "  {} ({})",
            a.provider.name,
            maestro_editor::agents::kind_label(a.kind())
        ),
    };
    if let Some(e) = &a.exited {
        tail.push_str(&format!("  process {e}"));
    }
    s.print(x, y, &tail, ui.text_muted, r);
    y += 1;
    if y >= area.bottom() {
        return;
    }
    // Row 2: what the status can be based on, or why there is none.
    let changed;
    let note = match (a.origin, a.kind()) {
        (Origin::Found, _) => {
            "no record: task and provider unknown; ret reviews it, a adopts it, A archives it"
        }
        (Origin::Composed, _) => {
            "composed result: no agent runs here; ret reviews it, t tests it, I integrates it like an agent's work"
        }
        _ if a.worktree_note.is_some() && !a.archived => {
            changed = format!(
                "worktree changed: {}",
                a.worktree_note.as_deref().unwrap_or_default()
            );
            changed.as_str()
        }
        (Origin::Loaded, _) if a.archived => {
            "archived: the worktree was removed; the record stays for its history and decisions"
        }
        (Origin::Loaded, _) if a.from_earlier_session() && a.base_status().is_decided() => {
            "no process, and none is started again: the work was decided on"
        }
        (Origin::Loaded, _) if a.from_earlier_session() => {
            "no process: R relaunches it; review, tests, comments, integrate and archive work without one"
        }
        (_, ProviderKind::Acp) => {
            "ACP: status comes from the agent's plan, tool, permission and turn messages"
        }
        (_, ProviderKind::Terminal) => {
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
    y += 1;
    if y >= area.bottom() {
        return;
    }
    // Row 5: the task contract, and what is outside it.
    draw_contract_row(ed, s, x0, y, r, a);
    y += 1;
    if y >= area.bottom() {
        return;
    }
    // Row 6: overlaps with other agents.
    draw_overlap_row(ed, s, x0, y, r, a);
}

/// `contract: ...`, led by a warning when changed files are outside it.
fn draw_contract_row(ed: &Editor, s: &mut Surface, x0: u16, y: u16, r: u16, a: &Agent) {
    let ui = &ed.theme.ui;
    let Some(c) = &a.contract else {
        s.print(x0, y, "no task contract", ui.text_muted, r);
        return;
    };
    let mut x = x0;
    let outside = a.out_of_scope();
    if !outside.is_empty() && !a.archived {
        let warn = format!("! {} changed file(s) outside the contract  ", outside.len());
        x = s.print(x, y, &warn, ui.message_warning.bold(), r);
    }
    s.print(x, y, &contract_line(c), ui.text_muted, r);
}

fn contract_line(c: &Contract) -> String {
    format!("contract: {}", c.brief())
}

fn draw_overlap_row(ed: &Editor, s: &mut Surface, x0: u16, y: u16, r: u16, a: &Agent) {
    let ui = &ed.theme.ui;
    let lines = conflicts::describe(ed, a.id);
    if lines.is_empty() {
        s.print(x0, y, "no overlap with other agents", ui.text_muted, r);
        return;
    }
    let conflict = conflicts::for_agent(ed, a.id)
        .iter()
        .any(|(_, p)| matches!(p.merge, conflicts::Merge::Conflicts(_)));
    let style = if conflict {
        ui.message_warning.bold()
    } else {
        ui.text
    };
    s.print(x0, y, &lines.join("; "), style, r);
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
    if a.origin == Origin::Found {
        push(
            "",
            "a found worktree with no record: nothing ran here in this session. v shows the history",
            ui.text_muted,
        );
    } else if a.origin == Origin::Composed {
        push(
            "",
            "a composed result: Maestro committed these hunks of other agents on their starting commit; no agent runs here",
            ui.text_muted,
        );
        for c in a.composition.iter().flat_map(|c| &c.sources) {
            push(
                "  ",
                &format!("{} from {} ({})", c.label, c.agent, c.test),
                ui.text,
            );
        }
    } else if a.from_earlier_session() && !a.can_relaunch() {
        push(
            "",
            "not running (from an earlier session); the transcript and screen are not saved",
            ui.text_muted,
        );
    } else if a.from_earlier_session() {
        let msg = match a.kind() {
            ProviderKind::Terminal => {
                "not running (from an earlier session). R starts its program again in the worktree, without the task"
            }
            ProviderKind::Acp => {
                "not running (from an earlier session); the transcript is not saved. R starts the agent again in the worktree"
            }
        };
        push("", msg, ui.text_muted);
    } else if a.kind() == ProviderKind::Terminal {
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
    if let Some(p) = a.permissions.first() {
        let warn = ui.text.patch(ed.theme.agent.awaiting_approval).bold();
        push("permission requested: ", &p.tool_title, warn);
        let opts: Vec<String> = p
            .options
            .iter()
            .map(|o| format!("{} ({})", o.label, o.kind.replace('_', " ")))
            .collect();
        push("choices: ", &opts.join(", "), warn);
        let more = a.permissions.len() - 1;
        let hint = if more > 0 {
            format!("p answers it; {more} more request(s) wait after it")
        } else {
            "p answers it".to_string()
        };
        push("", &hint, ui.text_muted);
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
        // A file outside the task contract is marked with `!` before its
        // change letter; the summary says what the mark means.
        let outside = a.contract.as_ref().is_some_and(|c| {
            maestro_editor::agents::file_paths(f)
                .iter()
                .any(|p| c.violation(p).is_some())
        });
        let x = if outside {
            s.print(
                list.x + 1,
                y,
                "!",
                bg.patch(ui.message_warning).bold(),
                name_r,
            )
        } else {
            list.x + 2
        };
        let x = s.print(x, y, kind_letter(&f.change), bg.bold(), name_r);
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
        let marked = match marks::describe(a) {
            Some(m) => format!("{m} (*)  "),
            None => String::new(),
        };
        let x = s.print(sm.x + 1, sm.y + 2, &marked, ui.text.bold(), r);
        let unsent = a.comments.iter().filter(|c| !c.sent).count();
        let state = if a.review.running {
            "refreshing".to_string()
        } else if let Some(t) = a.review.computed {
            format!("computed {}s ago (r refreshes)", t.elapsed().as_secs())
        } else {
            "not computed yet".to_string()
        };
        let text = format!("{unsent} unsent comment(s)  diff {state}");
        s.print(x, sm.y + 2, &text, ui.text_muted, r);
    }
    if sm.height > 3 {
        if let Some(e) = &a.review.error {
            s.print(
                sm.x + 1,
                sm.y + 3,
                &format!("diff failed: {e}"),
                ui.message_error,
                r,
            );
        } else if let Some(p) = a.parts.last() {
            let text = format!(
                "{} part(s) integrated on their own (=), the last on {}; the rest stays here",
                a.parts.len(),
                p.branch
            );
            s.print(sm.x + 1, sm.y + 3, &text, ui.text_muted, r);
        }
    }
    if sm.height > 4 {
        let outside = a.out_of_scope();
        match &a.contract {
            Some(_) if !outside.is_empty() => {
                let text = format!(
                    "! {} file(s) {} (marked !): integrating them needs O; D drops one",
                    outside.len(),
                    review::OUT_OF_SCOPE
                );
                s.print(sm.x + 1, sm.y + 4, &text, ui.message_warning.bold(), r);
            }
            Some(c) => {
                s.print(sm.x + 1, sm.y + 4, &contract_line(c), ui.text_muted, r);
            }
            None => {
                s.print(sm.x + 1, sm.y + 4, "no task contract", ui.text_muted, r);
            }
        }
    }
    if sm.height > 5 {
        draw_overlap_row(ed, s, sm.x + 1, sm.y + 5, r, a);
    }
    // Diff, with a gutter column for marks: `*` marked, `=` integrated,
    // `~` in the line range being chosen.
    let c = l.content;
    let rows = review::current_rows(ed);
    let file = files.get(v.file);
    let range = v.anchor.map(|a| (a.min(v.cursor), a.max(v.cursor)));
    for (i, row) in rows.iter().enumerate().skip(v.top).take(c.height as usize) {
        let y = c.y + (i - v.top) as u16;
        let area = Rect::new(c.x + 1, y, c.width.saturating_sub(1), 1);
        draw_row(ed, s, area, row, 0);
        if i == v.cursor {
            s.put(c.x, y, "▶", ui.border_active, c.x + 1);
        }
        let Some(f) = file else {
            continue;
        };
        let in_range = range.is_some_and(|(lo, hi)| (lo..=hi).contains(&i));
        match row {
            Row::Hunk { index, header } => {
                let state = match marks::hunk_state(a, f, *index) {
                    marks::HunkState::None => None,
                    marks::HunkState::Marked { taken, of } if taken == of => {
                        Some(("*", "[marked]".to_string()))
                    }
                    marks::HunkState::Marked { taken, of } => {
                        Some(("*", format!("[{taken} of {of} lines marked]")))
                    }
                    marks::HunkState::Integrated { part, whole: true } => {
                        Some(("=", format!("[integrated in part {part}]")))
                    }
                    marks::HunkState::Integrated { part, whole: false } => {
                        Some(("=", format!("[some lines integrated in part {part}]")))
                    }
                };
                if let Some((glyph, words)) = state {
                    s.put(c.x + 1, y, glyph, ui.text.bold(), c.x + 2);
                    let hx = c.x + 3 + review_width(header) as u16 + 2;
                    s.print(hx, y, &words, ui.text.bold(), c.right());
                }
            }
            Row::Line(rl) | Row::Pair(_, Some(rl)) | Row::Pair(Some(rl), None) => {
                // A side-by-side row with two changed lines shows the
                // stronger state of the two.
                let other = match row {
                    Row::Pair(Some(l), Some(_)) => Some(l),
                    _ => None,
                };
                let states: Vec<marks::LineState> = std::iter::once(rl)
                    .chain(other)
                    .map(|x| marks::line_state(a, f, x.hunk, x.line))
                    .collect();
                let glyph = if states.contains(&marks::LineState::Marked) {
                    Some("*")
                } else if states.contains(&marks::LineState::Integrated) {
                    Some("=")
                } else if in_range {
                    Some("~")
                } else {
                    None
                };
                if let Some(g) = glyph {
                    s.put(c.x + 1, y, g, ui.text.bold(), c.x + 2);
                }
            }
            _ => {}
        }
    }
}

/// Cells a hunk header takes.
fn review_width(text: &str) -> usize {
    panel::text_width(text)
}

// ----- compose ---------------------------------------------------------------

fn draw_compose(ed: &Editor, s: &mut Surface, l: &PanelLayout) {
    let ui = &ed.theme.ui;
    let d = &ed.theme.diff;
    let Some(v) = &ed.agents.panel.compose else {
        return;
    };
    let files = compose::files(ed, v);
    // File list, with how many hunks are marked in each.
    let list = l.list;
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
        let n = v
            .marks
            .iter()
            .filter(|(_, m)| m.pick.path() == Some(f.as_path()))
            .count();
        let tag = if n > 0 {
            format!(" {n} marked")
        } else {
            String::new()
        };
        let tw = tag.chars().count() as u16;
        let name_r = list.right().saturating_sub(tw);
        s.print(list.x + 2, y, &f.display().to_string(), bg, name_r);
        s.print(name_r, y, &tag, bg.bold(), list.right());
    }
    if files.is_empty() {
        s.print(
            list.x + 2,
            list.y,
            "no changed files yet",
            ui.text_muted,
            list.right(),
        );
    }
    let agents: Vec<&Agent> = v.agents.iter().filter_map(|i| ed.agents.get(*i)).collect();
    // Summary.
    let sm = l.summary;
    let r = sm.right();
    let task = agents
        .first()
        .and_then(|a| a.task.lines().next())
        .unwrap_or_default();
    let x = s.print(sm.x + 1, sm.y, "compose  ", ui.text.bold(), r);
    s.print(x, sm.y, task, ui.text, r);
    if sm.height > 1 {
        let base = agents
            .first()
            .and_then(|a| a.worktree.as_ref())
            .map(|w| w.base[..w.base.len().min(8)].to_string())
            .unwrap_or_default();
        let who: Vec<String> = agents
            .iter()
            .map(|a| format!("{} ({})", a.name, compose::test_label(a)))
            .collect();
        s.print(
            sm.x + 1,
            sm.y + 1,
            &format!("common starting commit {base}; agents: {}", who.join(", ")),
            ui.text_muted,
            r,
        );
    }
    if sm.height > 2 {
        let text = if v.marks.is_empty() {
            "no hunks marked: space marks the hunk under the cursor ([x] marked)".to_string()
        } else {
            let mut from: Vec<&str> = v
                .marks
                .iter()
                .map(|(_, m)| m.pick.source.as_str())
                .collect();
            from.sort();
            from.dedup();
            format!(
                "{} hunk(s) marked from {} ([x]): ret checks that they apply together and composes them",
                v.marks.len(),
                from.join(", ")
            )
        };
        s.print(sm.x + 1, sm.y + 2, &text, ui.text.bold(), r);
    }
    if sm.height > 3 {
        if v.checking {
            s.print(
                sm.x + 1,
                sm.y + 3,
                "checking whether the marked hunks apply together…",
                ui.text_muted,
                r,
            );
        } else if !v.problems.is_empty() {
            s.print(
                sm.x + 1,
                sm.y + 3,
                &format!(
                    "! {} marked hunk(s) do not apply together (marked [!] below; the status line says why)",
                    v.problems.len()
                ),
                ui.message_warning.bold(),
                r,
            );
        }
    }
    if sm.height > 4 {
        match agents.first().and_then(|a| a.contract.as_ref()) {
            Some(c) => s.print(sm.x + 1, sm.y + 4, &contract_line(c), ui.text_muted, r),
            None => s.print(sm.x + 1, sm.y + 4, "no task contract", ui.text_muted, r),
        };
    }
    if sm.height > 5 {
        s.print(
            sm.x + 1,
            sm.y + 5,
            "the agents' worktrees are not touched; the composed result gets a worktree of its own",
            ui.text_muted,
            r,
        );
    }
    // Columns: one per agent, its name and test result on top.
    let c = l.content;
    let n = agents.len().max(1) as u16;
    if c.height == 0 || c.width < n * 4 {
        return;
    }
    let colw = (c.width.saturating_sub(n - 1)) / n;
    let col_x = |i: usize| c.x + i as u16 * (colw + 1);
    for (i, a) in agents.iter().enumerate() {
        let x = col_x(i);
        let right = x + colw;
        let nx = s.print(x + 1, c.y, &a.name, ui.text.bold(), right);
        let style = match a.latest_test().and_then(|t| t.passed()) {
            Some(true) => ui.text.patch(ed.theme.agent.ready),
            Some(false) => ui.text.patch(ed.theme.agent.failed),
            None => ui.text_muted,
        };
        s.print(nx + 2, c.y, &compose::test_label(a), style, right);
        if i + 1 < agents.len() {
            for y in c.y..c.bottom() {
                s.put(right, y, "│", ui.border, right + 1);
            }
        }
    }
    let g = compose::grid(ed, v);
    let height = c.height.saturating_sub(1) as usize;
    for (ri, row) in g.rows.iter().enumerate().skip(v.top).take(height) {
        let y = c.y + 1 + (ri - v.top) as u16;
        for (ci, cell) in row.iter().enumerate() {
            let Some(cell) = cell else {
                continue;
            };
            let x = col_x(ci);
            let right = x + colw;
            match cell {
                compose::Cell::Head {
                    item,
                    header,
                    marked,
                    problem,
                } => {
                    let box_ = match (problem.is_some(), marked) {
                        (true, _) => "[!]",
                        (false, true) => "[x]",
                        (false, false) => "[ ]",
                    };
                    let style = if problem.is_some() {
                        ui.message_warning.bold()
                    } else {
                        ui.text.bold()
                    };
                    let bx = s.print(x + 1, y, box_, style, right);
                    s.print(bx + 1, y, header, d.hunk, right);
                    if *item == v.cursor {
                        s.put(x, y, "▶", ui.border_active, x + 1);
                    }
                }
                compose::Cell::Line { kind, text } => {
                    let rl = RowLine {
                        kind: *kind,
                        old: None,
                        new: None,
                        text: text.clone(),
                        marks: Vec::new(),
                        anchor: 0,
                        hunk: 0,
                        line: 0,
                    };
                    draw_side(
                        ed,
                        s,
                        Rect::new(x + 1, y, colw.saturating_sub(1), 1),
                        &rl,
                        0,
                    );
                }
                compose::Cell::Note(t) => {
                    s.print(x + 1, y, t, ui.text_muted, right);
                }
            }
        }
    }
    if g.rows.is_empty() {
        s.print(
            c.x + 1,
            c.y + 1,
            "no hunks in this file to compose",
            ui.text_muted,
            c.right(),
        );
    }
}

/// Draws one review row. `hscroll` is how many columns of each line's text are
/// scrolled off to the left (the proposal review scrolls sideways; the
/// agent review passes 0).
pub(crate) fn draw_row(ed: &Editor, s: &mut Surface, area: Rect, row: &Row, hscroll: usize) {
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
                hscroll,
            );
        }
        Row::Pair(left, right) => {
            let half = area.width.saturating_sub(1) / 2;
            let lx = area.x + 1;
            let rx = lx + half + 1;
            for (side, x, w, old_side) in [
                (left, lx, half, true),
                (right, rx, area.right().saturating_sub(rx), false),
            ] {
                if let Some(l) = side {
                    // Each side counts lines in its own file: a context
                    // line below an insertion has different numbers.
                    let n = if old_side { l.old } else { l.new };
                    let n = n.map(|n| n.to_string()).unwrap_or_default();
                    let nx = s.print(x, area.y, &format!("{n:>4} "), ui.text_muted, x + w);
                    draw_side(
                        ed,
                        s,
                        Rect::new(nx, area.y, (x + w).saturating_sub(nx), 1),
                        l,
                        hscroll,
                    );
                }
            }
            s.put(rx - 1, area.y, "│", ui.border, rx);
        }
    }
}

/// A sign and a line of text on its add or delete tint, with intraline
/// marks on the stronger tint. Text cut off at either edge is marked with
/// `<` or `>` in the edge cell, so a line never looks shorter than it is.
fn draw_side(ed: &Editor, s: &mut Surface, area: Rect, l: &RowLine, left: usize) {
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
    let edge = ui.text.patch(sign_style).bold();
    let mut x = tx;
    let mut col = 0usize;
    let mut hidden_left = false;
    for (at, g) in l.text.grapheme_indices(true) {
        let marked = l.marks.iter().any(|m| at >= m.start && at < m.end);
        let style = if marked {
            base.patch(Style::bg(mark_bg))
        } else {
            base
        };
        // Columns this grapheme takes, as the surface draws it.
        let w = if g == "\t" {
            4 - col % 4
        } else {
            maestro_editor::agents::panel::cell_width(g)
        };
        if col + w <= left {
            col += w;
            hidden_left = true;
            continue;
        }
        // A tab, or a grapheme partly scrolled off, is drawn as blanks.
        let partial = col < left;
        hidden_left |= partial;
        let blanks = g == "\t" || partial;
        let need = if blanks { col + w - left.max(col) } else { w };
        if x as usize + need > area.right() as usize {
            s.put(
                area.right().saturating_sub(1),
                area.y,
                ">",
                edge,
                area.right(),
            );
            break;
        }
        if blanks {
            for _ in 0..need {
                x += s.put(x, area.y, " ", style, area.right());
            }
        } else {
            x += s.put(x, area.y, g, style, area.right());
        }
        col += w;
    }
    if hidden_left {
        s.put(tx, area.y, "<", edge, area.right());
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
            let forced = matches!(c.action, ConfirmAction::ForceArchive(_));
            let title = if forced {
                format!("{}: this deletes files", c.title)
            } else {
                c.title.clone()
            };
            draw_dialog(ed, s, area, &title, &c.lines, &c.keys);
            None
        }
    }
}

/// A confirmation dialog: wrapped text and the keys that answer it. The
/// editor wraps the text the same way, so it knows whether the dialog fits
/// and refuses to accept one that does not.
pub(crate) fn draw_dialog(
    ed: &Editor,
    s: &mut Surface,
    area: Rect,
    title: &str,
    text: &[String],
    keys: &[(String, String)],
) {
    let ui = &ed.theme.ui;
    let (w, _) = panel::dialog_width(area);
    let x = area.x + (area.width - w) / 2;
    let mut lines: Vec<(String, Style)> = panel::dialog_lines(area, text)
        .into_iter()
        .map(|l| (l, ui.popup))
        .collect();
    if !panel::dialog_fits(area, text, keys) {
        let room = (area.height as usize).saturating_sub(6);
        lines.truncate(room);
        lines.push((
            "the window is too small to show all of this; enlarge it to confirm".into(),
            ui.popup_title,
        ));
    }
    let h = (lines.len() as u16 + 4)
        .min(area.height.saturating_sub(1))
        .max(4);
    let y = area.y + (area.height.saturating_sub(h)) / 3;
    let b = Rect::new(x, y, w, h);
    draw_box(s, b, title, ui.popup, ui.popup_border, ui.popup_title);
    let rows = (h - 4) as usize;
    for (i, (text, style)) in lines.iter().take(rows).enumerate() {
        s.print(x + 2, y + 1 + i as u16, text, *style, b.right() - 1);
    }
    let ky = b.bottom() - 2;
    let mut kx = x + 2;
    for (k, d) in keys {
        kx = s.print(kx, ky, k, ui.popup_title, b.right() - 1);
        kx = s.print(kx + 1, ky, d, ui.popup, b.right() - 1);
        kx += 3;
    }
}

/// An input box with a label, drawn like the panel's.
pub(crate) fn draw_input(
    ed: &Editor,
    s: &mut Surface,
    area: Rect,
    label: &str,
    text: &str,
) -> CursorShape {
    let ui = &ed.theme.ui;
    let w = area.width.saturating_sub(4).min(90);
    let inner = w.saturating_sub(4) as usize;
    let x = area.x + (area.width - w) / 2;
    let lines = wrap(text, inner.max(1));
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
    CursorShape::Bar(end.0.min(b.right() - 2), end.1)
}

#[cfg(test)]
#[path = "agents_tests.rs"]
mod tests;
