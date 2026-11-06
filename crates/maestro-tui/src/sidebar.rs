//! Draws the workspaces sidebar (`space s`) in its column left of the
//! views. Reads the editor; never changes it.
//!
//! Every agent state is a mark and a word (`! ready for review`), drawn in
//! the agent panel's status color; the color is never the only signal. The
//! selected row uses the picker's selection colors for all its text.

use maestro_config::Style;
use maestro_editor::workspaces::sidebar::{self, Row};
use maestro_editor::workspaces::{AgentEntry, WorktreeRow};
use maestro_editor::{Editor, Rect};

use crate::surface::Surface;

/// One drawn line: segments of text and their styles.
type Line = Vec<(String, Style)>;

pub(crate) fn draw(ed: &Editor, s: &mut Surface, area: Rect) {
    let ui = &ed.theme.ui;
    let focused = ed.workspaces.sidebar.focused;
    s.fill(area, ui.text);
    // The divider between the sidebar and the views.
    let x = area.right();
    let border = if focused { ui.border_active } else { ui.border };
    for y in area.y..area.bottom() {
        s.put(x, y, "│", border, x + 1);
    }
    if area.height == 0 || area.width == 0 {
        return;
    }
    let title_style = if focused {
        ui.tab_active
    } else {
        ui.tab_inactive
    };
    s.fill(Rect::new(area.x, area.y, area.width, 1), title_style);
    let title = match maestro_editor::workspaces::status_label(ed) {
        Some(l) => format!(" workspaces  in {l}"),
        None => " workspaces".to_string(),
    };
    s.print(area.x, area.y, &title, title_style, area.right());

    let footer = focused && area.height > 4;
    let body = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(1 + u16::from(footer)),
    );
    if footer {
        let y = area.bottom() - 1;
        // In a narrow column the fold keys are left out; the unknown-key
        // message still lists them.
        let full = " ret open  h/l fold  a add project  esc editor";
        let short = " ret open  a add project  esc editor";
        let hint = if full.chars().count() <= area.width as usize {
            full
        } else {
            short
        };
        s.print(area.x, y, hint, ui.text_muted, area.right());
    }

    let rows = sidebar::rows(ed);
    let selected = sidebar::selected_index(ed, &rows);
    let mut lines: Vec<(usize, Line)> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        for l in row_lines(ed, row) {
            lines.push((i, l));
        }
    }
    // Keep the selected row on screen.
    let height = body.height as usize;
    let mut offset = 0;
    if let Some(sel) = selected {
        let first = lines.iter().position(|(i, _)| *i == sel).unwrap_or(0);
        let last = lines.iter().rposition(|(i, _)| *i == sel).unwrap_or(first);
        if last + 1 > height {
            offset = last + 1 - height;
        }
        offset = offset.min(first);
    }
    for (n, (row, line)) in lines.iter().skip(offset).take(height).enumerate() {
        let y = body.y + n as u16;
        let is_selected = Some(*row) == selected;
        let highlight = if focused {
            ui.popup_selected
        } else {
            ui.text.patch(ui.cursorline)
        };
        if is_selected {
            s.fill(Rect::new(body.x, y, body.width, 1), highlight);
        }
        let mut cx = body.x;
        for (text, style) in line {
            let style = if is_selected {
                if focused {
                    highlight
                } else {
                    style.patch(ui.cursorline)
                }
            } else {
                *style
            };
            cx = s.print(cx, y, text, style, body.right());
            if cx >= body.right() {
                break;
            }
        }
    }
}

fn row_lines(ed: &Editor, row: &Row) -> Vec<Line> {
    let ui = &ed.theme.ui;
    match row {
        Row::Heading(t) => vec![vec![(format!(" {t}"), ui.popup_title)]],
        Row::Note(t) => vec![vec![(format!("   {t}"), ui.text_muted)]],
        Row::Agent(a) => {
            let style = crate::agents::status_style(&ed.theme, a.status);
            let mut line = vec![
                (format!("  {} ", a.attention.symbol()), style),
                (a.name.clone(), style),
                (format!("  {}", a.attention.word()), style),
            ];
            if let Some(p) = a.project.as_deref().and_then(|p| p.file_name()) {
                line.push((format!("  {}", p.to_string_lossy()), ui.text_muted));
            }
            if a.from_records {
                line.push((" (earlier session)".into(), ui.text_muted));
            }
            vec![line]
        }
        Row::Project {
            name,
            expanded,
            worktrees,
            current,
            error,
            ..
        } => {
            let mark = if *expanded { "v" } else { ">" };
            let mut detail = match error {
                Some(_) => "  not available".to_string(),
                None => format!(
                    "  {worktrees} worktree{}",
                    if *worktrees == 1 { "" } else { "s" }
                ),
            };
            if *current {
                detail.push_str(", current");
            }
            vec![vec![
                (format!(" {mark} {name}"), ui.text),
                (detail, ui.text_muted),
            ]]
        }
        Row::Worktree {
            row,
            current,
            unsaved,
            agent,
            ..
        } => worktree_lines(ed, row, *current, *unsaved, agent.as_ref()),
    }
}

fn worktree_lines(
    ed: &Editor,
    w: &WorktreeRow,
    current: bool,
    unsaved: usize,
    agent: Option<&AgentEntry>,
) -> Vec<Line> {
    let ui = &ed.theme.ui;
    let mark = if current { "@" } else { " " };
    let mut first = vec![(format!("   {mark} {}", w.branch_label()), ui.text)];
    if current {
        first.push(("  here".into(), ui.text_muted));
    }
    let mut facts = Vec::new();
    facts.push(if w.is_main {
        "main worktree".to_string()
    } else {
        w.name.clone()
    });
    if w.missing {
        facts.push("missing".into());
    } else {
        match (w.dirty, &w.error) {
            (Some(0), _) => facts.push("clean".into()),
            (Some(n), _) => facts.push(format!("{n} changed")),
            (None, Some(e)) => facts.push(e.clone()),
            (None, None) => {}
        }
    }
    if let Some(u) = &w.upstream {
        facts.push(format!("↑{} ↓{}", u.ahead, u.behind));
    }
    if w.locked {
        facts.push("locked".into());
    }
    if unsaved > 0 {
        facts.push(format!("{unsaved} unsaved"));
    }
    let mut out = vec![
        first,
        vec![(format!("     {}", facts.join(", ")), ui.text_muted)],
    ];
    match agent {
        Some(a) => {
            let style = crate::agents::status_style(&ed.theme, a.status);
            out.push(vec![
                (format!("     {} ", a.attention.symbol()), style),
                (format!("{} {}", a.name, a.attention.word()), style),
            ]);
        }
        None if w.is_agent_branch() => out.push(vec![(
            "     agent branch, no agent record".into(),
            ui.text_muted,
        )]),
        None => {}
    }
    out
}

#[cfg(test)]
#[path = "sidebar_tests.rs"]
mod tests;
