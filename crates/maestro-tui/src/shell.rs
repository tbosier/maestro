//! Draws the shell pane (`space o`, see `maestro_editor::shell`): a header
//! naming the directory the shell started in and who has the keys, and the
//! shell's screen under it.

use maestro_editor::shell::{header, screen_area};
use maestro_editor::{Editor, Rect};
use unicode_segmentation::UnicodeSegmentation;

use crate::agents::draw_screen;
use crate::render::CursorShape;
use crate::surface::{Surface, grapheme_width, text_width};

/// Columns the header keeps for `shell` and the end of the directory
/// before it takes a shorter badge.
const TITLE_MIN: usize = 32;

/// Draws the pane in `area`. Returns where the terminal cursor goes while
/// the shell has the keys.
pub(crate) fn draw(ed: &Editor, s: &mut Surface, area: Rect) -> Option<CursorShape> {
    let ui = &ed.theme.ui;
    if area.height == 0 || area.width == 0 {
        return None;
    }
    s.fill(area, ui.text);
    let h = Rect::new(area.x, area.y, area.width, 1);
    s.fill(h, ui.statusline);
    let header = header(ed);
    let typing = header.typing;
    let style = if typing {
        ui.mode_insert
    } else {
        ui.mode_normal
    };
    // The longest badge that leaves the title its whole width, or at least
    // TITLE_MIN columns; else the shortest. A directory that does not fit
    // is cut from the left, so its last components stay.
    let title = format!(" shell  {} ", header.dir);
    let width = usize::from(h.width);
    let title_w = text_width(&title);
    let badge = header
        .badges
        .iter()
        .find(|b| text_width(b) + title_w.min(TITLE_MIN) <= width)
        .or(header.badges.last())
        .cloned()
        .unwrap_or_default();
    let bx = h.right().saturating_sub(text_width(&badge) as u16).max(h.x);
    s.print(bx, h.y, &badge, style, h.right());
    let room = usize::from(bx - h.x);
    let title = if title_w <= room {
        title
    } else {
        // " shell  " and a space after the directory stay.
        format!(" shell  {} ", tail(&header.dir, room.saturating_sub(9)))
    };
    s.print(h.x, h.y, &title, ui.statusline.bold(), bx);

    let session = ed.shell.session()?;
    let body = screen_area(area);
    let screen = session.terminal.snapshot();
    draw_screen(&ed.theme, s, body, &screen);
    if screen.scrolled && body.height > 0 {
        let msg = if typing {
            " scrolled back: typing returns "
        } else {
            " scrolled back: the wheel returns "
        };
        // Never left of the pane: a narrow pane cuts the notice instead.
        s.print(
            body.right().saturating_sub(msg.len() as u16).max(body.x),
            body.y,
            msg,
            ui.mode_command,
            body.right(),
        );
    }
    if typing
        && let Some((row, col)) = screen.cursor
        && row < body.height
        && col < body.width
    {
        return Some(CursorShape::Bar(body.x + col, body.y + row));
    }
    None
}

/// The end of `text` in at most `width` columns, after a `…` when cut.
fn tail(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut kept = Vec::new();
    let mut used = 1;
    for g in text.graphemes(true).rev() {
        let w = grapheme_width(g);
        if used + w > width {
            break;
        }
        used += w;
        kept.push(g);
    }
    kept.reverse();
    format!("…{}", kept.concat())
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
