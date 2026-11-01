//! Draws the proposal review overlay over the editor area: the queue on
//! the left, the selected proposal's facts and its diff on the right, in
//! the agent review screen's layout and colors. Reads the editor; never
//! changes it.

use maestro_config::{Style, Theme};
use maestro_editor::Editor;
use maestro_editor::Rect;
use maestro_editor::agents::panel;
use maestro_editor::proposals::review::{self, Dialog};
use maestro_editor::proposals::{Decision, Proposal, State, VerifyStatus};

use crate::agents::{draw_dialog, draw_input, draw_row};
use crate::render::CursorShape;
use crate::surface::Surface;

/// The word for where a proposal is, and its color. The word is always
/// drawn with the color.
pub fn state_word(theme: &Theme, p: &Proposal) -> (&'static str, Style) {
    let a = &theme.agent;
    match (p.state, p.decision) {
        (State::Pending, _) => ("pending", a.awaiting_approval),
        (State::Verifying, _) => ("verifying", a.running),
        (_, Some(Decision::Approved)) => ("approved", a.completed),
        (_, Some(Decision::Rejected)) => ("rejected", a.rejected),
        (_, Some(Decision::Withdrawn)) => ("withdrawn", a.starting),
        (_, Some(Decision::Stale)) => ("stale", a.failed),
        (_, Some(Decision::Failed)) => ("failed", a.failed),
        (_, None) => ("resolved", a.starting),
    }
}

fn verify_style(theme: &Theme, s: VerifyStatus) -> Style {
    let a = &theme.agent;
    match s {
        VerifyStatus::Passed => a.completed,
        VerifyStatus::Failed | VerifyStatus::Error => a.failed,
        VerifyStatus::Running => a.running,
        VerifyStatus::NotRun => a.starting,
    }
}

pub(crate) fn draw(ed: &Editor, s: &mut Surface) -> CursorShape {
    let area = ed.area;
    let ui = &ed.theme.ui;
    if area.width < 20 || area.height < 6 {
        s.fill(area, ui.text);
        s.print(
            area.x,
            area.y,
            "proposals: the window is too small",
            ui.message_warning,
            area.right(),
        );
        return CursorShape::Hidden;
    }
    let Some(v) = &ed.proposals.review else {
        return CursorShape::Hidden;
    };
    let l = panel::layout(area);
    s.fill(area, ui.text);

    // Header.
    let h = l.header;
    s.fill(h, ui.statusline);
    let title = format!(
        " edit proposals: {} pending, {} total ",
        ed.proposals.pending_count(),
        ed.proposals.items.len()
    );
    let x = s.print(h.x, h.y, &title, ui.statusline.bold(), h.right());
    let badge = " KEYS: PROPOSALS ";
    let bw = badge.chars().count() as u16;
    s.print(
        h.right().saturating_sub(bw).max(x + 1),
        h.y,
        badge,
        ui.mode_normal,
        h.right(),
    );
    for y in l.divider.y..l.divider.bottom() {
        s.put(l.divider.x, y, "│", ui.border, l.divider.right());
    }

    // The queue.
    let list = l.list;
    let items = &ed.proposals.items;
    let at = v
        .selected
        .and_then(|id| items.iter().position(|p| p.id == id))
        .unwrap_or(0);
    let visible = list.height as usize;
    let first = if at >= visible { at + 1 - visible } else { 0 };
    for (i, p) in items.iter().enumerate().skip(first).take(visible) {
        let y = list.y + (i - first) as u16;
        let selected = Some(p.id) == v.selected;
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
        let (word, style) = state_word(&ed.theme, p);
        let mut x = s.print(
            list.x + 2,
            y,
            &format!("#{}", p.id.0),
            bg.bold(),
            list.right(),
        );
        x = s.print(x + 1, y, word, bg.patch(style), list.right());
        s.print(x + 1, y, &p.actor.name, bg, list.right());
    }

    // Facts about the selected proposal.
    let sm = l.summary;
    let r = sm.right();
    match review::selected(ed) {
        None => {
            s.print(sm.x + 1, sm.y, "no proposal selected", ui.text_muted, r);
        }
        Some(p) => draw_summary(ed, s, sm, p),
    }

    // The diff, or the verification output.
    let c = l.content;
    let rows = review::current_rows(ed);
    for (i, row) in rows.iter().enumerate().skip(v.top).take(c.height as usize) {
        let y = c.y + (i - v.top) as u16;
        draw_row(ed, s, Rect::new(c.x, y, c.width, 1), row, v.left);
        if i == v.cursor {
            s.put(c.x, y, "▶", ui.border_active, c.x + 1);
        }
    }
    if rows.is_empty() && c.height > 0 {
        s.print(c.x + 2, c.y, "no change to show", ui.text_muted, c.right());
    }

    // Key hints.
    let hl = l.hints;
    if hl.height > 0 {
        s.fill(hl, ui.text);
        let mut x = hl.x + 1;
        for (k, d) in review::hints(ed) {
            let need = (k.chars().count() + d.chars().count() + 3) as u16;
            if x + need > hl.right() {
                break;
            }
            x = s.print(x, hl.y, k, ui.text.bold(), hl.right());
            x = s.print(x + 1, hl.y, d, ui.text_muted, hl.right());
            x += 2;
        }
    }

    match &v.dialog {
        Some(Dialog::Approve {
            title, lines, keys, ..
        }) => {
            draw_dialog(ed, s, area, title, lines, keys);
            CursorShape::Hidden
        }
        Some(Dialog::Reject { id, text }) => draw_input(
            ed,
            s,
            area,
            &format!("reason for rejecting proposal {} (optional)", id.0),
            text,
        ),
        None => CursorShape::Hidden,
    }
}

fn draw_summary(ed: &Editor, s: &mut Surface, sm: Rect, p: &Proposal) {
    let ui = &ed.theme.ui;
    let r = sm.right();
    let (word, style) = state_word(&ed.theme, p);
    let mut x = s.print(
        sm.x + 1,
        sm.y,
        &format!("proposal {}", p.id.0),
        ui.text.bold(),
        r,
    );
    x = s.print(x + 2, sm.y, word, ui.text.patch(style), r);
    x = s.print(x + 2, sm.y, &p.actor.label(), ui.text, r);
    if let Some(reason) = &p.reason {
        s.print(x + 2, sm.y, &format!("reason: {reason}"), ui.text_muted, r);
    }
    if sm.height > 1 {
        let (add, rem) = p.counts();
        let current = ed.buffers.get(&p.doc).map(|b| b.doc.version().0);
        let versions = match current {
            Some(c) => format!("base version {}, document at {c}", p.base_version.0),
            None => format!("base version {}, document closed", p.base_version.0),
        };
        // The versions first: whether it can still apply matters more than
        // the path, which the approval dialog shows in full.
        let line = format!("{versions}  {} edit(s) +{add} -{rem}", p.edit_count);
        let mut x = s.print(sm.x + 1, sm.y + 1, &line, ui.text, r);
        let moved = current != Some(p.base_version.0);
        if p.state == State::Pending && moved {
            x = s.print(
                x + 2,
                sm.y + 1,
                "stale: approving will refuse it",
                ui.message_error,
                r,
            );
        }
        s.print(x + 2, sm.y + 1, &p.file_label(), ui.text_muted, r);
    }
    if sm.height > 2 {
        s.print(sm.x + 1, sm.y + 2, &p.description, ui.text, r);
    }
    if sm.height > 3 {
        let y = sm.y + 3;
        match (&p.verify, &p.result) {
            (None, _) => {
                let text = if p.state == State::Pending {
                    "no verification command; approving does not save the file"
                } else {
                    "no verification command"
                };
                s.print(sm.x + 1, y, text, ui.text_muted, r);
            }
            (Some(v), None) => {
                s.print(
                    sm.x + 1,
                    y,
                    &format!(
                        "verify after saving: {} (in {})",
                        v.display(),
                        v.cwd.display()
                    ),
                    ui.text_muted,
                    r,
                );
            }
            (Some(v), Some(res)) => {
                let x = s.print(sm.x + 1, y, "verify ", ui.text_muted, r);
                let x = s.print(
                    x,
                    y,
                    &res.summary(),
                    ui.text.patch(verify_style(&ed.theme, res.status)),
                    r,
                );
                s.print(x + 2, y, &v.display(), ui.text_muted, r);
            }
        }
    }
}

#[cfg(test)]
#[path = "proposals_tests.rs"]
mod tests;
