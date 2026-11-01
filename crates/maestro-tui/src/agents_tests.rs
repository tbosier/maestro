//! Tests for the agent panel drawing (`agents.rs`).

use super::*;
use maestro_config::Config;
use maestro_editor::agents::TermCell;
use maestro_editor::{KeyCode, KeyEvent};

fn row(s: &Surface, y: u16) -> String {
    s.row(y).iter().map(|c| c.symbol.as_str()).collect()
}

#[test]
fn empty_panel_says_who_has_the_keys_and_what_to_press() {
    let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 100, 28));
    ed.handle_key(KeyEvent::char(' '));
    ed.handle_key(KeyEvent::char('a'));
    assert!(ed.agents.panel_open());
    let mut s = Surface::new(100, 30, ed.theme.ui.text);
    crate::render::render(&ed, &mut s);
    assert!(row(&s, 1).contains("agents: 0 total"), "{:?}", row(&s, 1));
    assert!(row(&s, 1).contains("KEYS: PANEL"));
    assert!((2..6).any(|y| row(&s, y).contains("No agents")));
    let hints = row(&s, 28);
    assert!(hints.contains("n new task"), "{hints:?}");
    // `n` asks for the task in a box with a cursor.
    ed.handle_key(KeyEvent::char('n'));
    ed.paste_text("fix it");
    let mut s = Surface::new(100, 30, ed.theme.ui.text);
    let cursor = crate::render::render(&ed, &mut s);
    assert!((0..30).any(|y| row(&s, y).contains("fix it")));
    assert!(matches!(cursor, CursorShape::Bar(..)));
    assert!(row(&s, 28).contains("ret submit"));
    // Escape closes the box, then the panel; the editor is back.
    ed.handle_key(KeyEvent::plain(KeyCode::Esc));
    ed.handle_key(KeyEvent::plain(KeyCode::Esc));
    assert!(!ed.agents.panel_open());
}

#[test]
fn panel_never_panics_at_small_sizes() {
    for (w, h) in (1..=60u16).flat_map(|w| (1..=14u16).map(move |h| (w, h))) {
        let mut ed = Editor::new(Config::default(), Rect::new(0, 1, w, h.saturating_sub(2)));
        ed.handle_key(KeyEvent::char(' '));
        ed.handle_key(KeyEvent::char('a'));
        ed.handle_key(KeyEvent::char('n'));
        let mut s = Surface::new(w, h, ed.theme.ui.text);
        crate::render::render(&ed, &mut s);
    }
}

#[test]
fn terminal_cells_keep_their_colors() {
    let ed = Editor::new(Config::default(), Rect::new(0, 1, 40, 10));
    let cell = |text: &str, fg, bg, inverse| TermCell {
        text: text.into(),
        fg,
        bg,
        inverse,
        ..TermCell::default()
    };
    let screen = Screen {
        rows: 1,
        cols: 4,
        cells: vec![
            cell("a", TermColor::Default, TermColor::Default, false),
            cell("b", TermColor::Indexed(1), TermColor::Default, false),
            cell("c", TermColor::Rgb(1, 2, 3), TermColor::Indexed(196), false),
            cell("d", TermColor::Indexed(2), TermColor::Default, true),
        ],
        cursor: None,
        scrolled: false,
    };
    let mut s = Surface::new(10, 2, ed.theme.ui.text);
    draw_screen(&ed.theme, &mut s, Rect::new(0, 0, 10, 2), &screen);
    let t = &ed.theme;
    let at = |x| s.cell(x, 0).unwrap().clone();
    assert_eq!(at(0).symbol, "a");
    assert_eq!(at(0).style.fg, t.ui.text.fg);
    assert_eq!(at(0).style.bg, Some(t.background));
    assert_eq!(at(1).style.fg, Some(t.terminal[1]));
    assert_eq!(at(2).style.fg, Some(Rgb(1, 2, 3)));
    assert_eq!(at(2).style.bg, Some(Rgb(255, 0, 0)));
    // Inverse swaps them.
    assert_eq!(at(3).style.fg, Some(t.background));
    assert_eq!(at(3).style.bg, Some(t.terminal[2]));
}

fn record(id: u64, status: &str) -> maestro_editor::agents::persist::AgentRecord {
    use maestro_editor::agents::persist::{AgentRecord, SavedWorktree};
    AgentRecord {
        id,
        name: format!("sh-{id}"),
        group: id,
        task: "write the notes".into(),
        provider: "sh".into(),
        kind: "terminal".into(),
        worktree: SavedWorktree {
            path: format!("/nonexistent/repo-worktrees/notes-{id}"),
            branch: format!("maestro/notes-{id}"),
            base: "0123456789abcdef".into(),
            main_root: "/nonexistent/repo".into(),
        },
        status: status.into(),
        archived: false,
        exited: None,
        errors: vec![],
        history: vec![],
        comments: vec![],
        tests: vec![],
        session_id: None,
        load_session: false,
        created: 0,
    }
}

fn screen(ed: &Editor) -> Vec<String> {
    let mut s = Surface::new(120, 30, ed.theme.ui.text);
    crate::render::render(ed, &mut s);
    (0..30).map(|y| row(&s, y)).collect()
}

#[test]
fn loaded_agents_say_they_are_not_running_and_found_worktrees_have_no_status() {
    use maestro_editor::agents::{Origin, persist::from_record};
    let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 120, 28));
    // Saved while running: it loads as stopped, never as running.
    ed.agents
        .agents
        .push(from_record(&record(1, "running"), &[]));
    let mut found = from_record(&record(2, "stopped"), &[]);
    found.origin = Origin::Found;
    found.name = "found-notes-2".into();
    ed.agents.agents.push(found);
    ed.handle_key(KeyEvent::char(' '));
    ed.handle_key(KeyEvent::char('a'));
    let rows = screen(&ed);
    let text = rows.join("\n");
    let first = rows.iter().find(|r| r.contains("sh-1")).unwrap();
    assert!(first.starts_with("▌ sh-1  stopped  "), "{first}");
    assert!(
        rows.iter()
            .any(|r| r.contains("sh-1  stopped  not running (from an earlier session)")),
        "{text}"
    );
    assert!(text.contains("not running, earlier session"), "{text}");
    assert!(
        text.contains("R relaunch") && text.contains("A archive"),
        "{text}"
    );
    assert!(!text.contains("f feedback"), "feedback cannot reach it");
    // The found worktree shows that it has no record instead of a status.
    ed.handle_key(KeyEvent::char('j'));
    let text = screen(&ed).join("\n");
    assert!(text.contains("found-notes-2  found worktree"), "{text}");
    assert!(
        text.contains("no record: task and provider unknown"),
        "{text}"
    );
    assert!(text.contains("a adopt"), "{text}");
    assert!(!text.contains("R relaunch"), "{text}");
}
