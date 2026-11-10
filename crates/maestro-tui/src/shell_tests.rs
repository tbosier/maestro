//! Drawing tests for the shell pane, with a real `sh` in a terminal.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use maestro_config::Config;
use maestro_editor::shell::{MIN_ROWS, VIEWS_MIN_ROWS};
use maestro_editor::{Editor, KeyCode, KeyEvent, Rect};

use crate::render::{CursorShape, render, split_screen};
use crate::surface::Surface;

fn row(s: &Surface, y: u16) -> String {
    s.row(y).iter().map(|c| c.symbol.as_str()).collect()
}

fn poll_until(ed: &mut Editor, what: &str, mut done: impl FnMut(&Editor) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        ed.poll_background();
        if done(ed) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; status {:?}",
            ed.status
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn type_text(ed: &mut Editor, text: &str) {
    for c in text.chars() {
        let key = if c == '\n' {
            KeyEvent::plain(KeyCode::Enter)
        } else {
            KeyEvent::char(c)
        };
        ed.handle_key(key);
    }
}

/// The text the shell shows, row by row.
fn shell_text(ed: &Editor) -> String {
    let Some(s) = ed.shell.session() else {
        return String::new();
    };
    let screen = s.terminal.snapshot();
    (0..screen.rows)
        .map(|r| screen.row_text(r))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A directory under `$HOME` when the crate sits under it (as checkouts
/// usually do), so the header has a home directory to abbreviate.
fn crate_dir() -> PathBuf {
    std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap()
}

/// An editor for a `w` x `h` screen in `dir`, with the shell pane opened by
/// `space o` running `sh`.
fn editor_with_shell(w: u16, h: u16, dir: PathBuf) -> Editor {
    let mut ed = Editor::new(Config::default(), Rect::new(0, 1, w, h - 2));
    let (_, region, _) = split_screen(w, h, &ed);
    ed.resize(region);
    ed.cwd = dir;
    ed.shell.program = Some("sh".into());
    type_text(&mut ed, " o");
    assert!(ed.shell.has_focus(), "status {:?}", ed.status);
    ed
}

#[test]
fn the_pane_draws_its_header_and_the_shell_under_the_views() {
    let (w, h) = (220, 30);
    let dir = crate_dir();
    let mut ed = editor_with_shell(w, h, dir.clone());
    type_text(&mut ed, "printf 'hi-from-%s\\n' shell\n");
    poll_until(&mut ed, "the shell's output", |ed| {
        shell_text(ed).contains("hi-from-shell")
    });

    let pane = ed.shell.area().expect("the pane is shown");
    // A third of the region, under the views, as wide as they are.
    let region = ed.region();
    assert_eq!(pane.height, region.height / 3);
    assert_eq!(pane.bottom(), region.bottom());
    assert_eq!(ed.area.bottom(), pane.y);
    assert_eq!((pane.x, pane.width), (region.x, region.width));

    let mut s = Surface::new(w, h, ed.theme.ui.text);
    let cursor = render(&ed, &mut s);
    let header = row(&s, pane.y);
    assert!(header.starts_with(" shell  "), "{header:?}");
    assert!(
        header.contains("KEYS: TYPING INTO SHELL (C-\\ or a click outside returns)"),
        "{header:?}"
    );
    let shown = maestro_editor::navigation::home_path(&dir);
    assert!(header.contains(&shown), "{header:?} should name {shown}");
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && home.parent().is_some()
        && dir.starts_with(&home)
    {
        let home = home.to_string_lossy().into_owned();
        assert!(!header.contains(&home), "{header:?} shows {home}");
        assert!(shown.starts_with("~/"), "{shown}");
    }
    let body: Vec<String> = (pane.y + 1..pane.bottom()).map(|y| row(&s, y)).collect();
    assert!(
        body.iter().any(|r| r.trim_end() == "hi-from-shell"),
        "{body:#?}"
    );
    // The terminal's cursor sits in the pane while the shell has the keys.
    match cursor {
        CursorShape::Bar(x, y) => {
            assert!(pane.contains(x, y) && y > pane.y, "{cursor:?} in {pane:?}");
        }
        CursorShape::Hidden => panic!("no cursor while typing into the shell"),
    }

    // C-\ gives the keys back; the header says so and the pane stays.
    ed.handle_key(KeyEvent::ctrl('\\'));
    let mut s = Surface::new(w, h, ed.theme.ui.text);
    let cursor = render(&ed, &mut s);
    let header = row(&s, pane.y);
    assert!(
        header.contains("KEYS: EDITOR (C-\\ or a click"),
        "{header:?}"
    );
    assert!(
        !matches!(cursor, CursorShape::Bar(_, y) if y >= pane.y),
        "{cursor:?}"
    );
}

#[test]
fn a_narrow_header_keeps_the_end_of_the_directory_and_a_short_badge() {
    // A directory longer than the pane is wide.
    let dir = crate_dir().join("src");
    let mut ed = editor_with_shell(40, 30, dir.clone());
    poll_until(&mut ed, "the prompt", |ed| {
        !shell_text(ed).trim().is_empty()
    });
    let pane = ed.shell.area().unwrap();
    let mut s = Surface::new(40, 30, ed.theme.ui.text);
    render(&ed, &mut s);
    let header = row(&s, pane.y);
    assert!(header.ends_with(" SHELL "), "{header:?}");
    assert!(header.starts_with(" shell  "), "{header:?}");
    let shown = maestro_editor::navigation::home_path(&dir);
    if shown.chars().count() > 20 {
        assert!(header.contains("…"), "{header:?}");
    }
    assert!(header.contains("maestro-tui/src "), "{header:?}");
}

#[test]
fn after_exit_the_header_gives_the_status_even_with_a_long_directory() {
    // Long enough that the whole title and the longest badge cannot share
    // 80 columns.
    let dir = crate_dir().join("src");
    let mut ed = editor_with_shell(80, 30, dir);
    poll_until(&mut ed, "the prompt", |ed| {
        !shell_text(ed).trim().is_empty()
    });
    type_text(&mut ed, "exit 3\n");
    poll_until(&mut ed, "the exit", |ed| {
        ed.shell.session().is_some_and(|s| s.exited.is_some())
    });
    let pane = ed.shell.area().expect("the pane stays after the exit");
    let mut s = Surface::new(80, 30, ed.theme.ui.text);
    render(&ed, &mut s);
    let header = row(&s, pane.y);
    assert!(header.contains(" exited with code 3"), "{header:?}");
    assert!(header.contains("src "), "{header:?}");
    // While typing, the way out stays on the header too.
    let mut ed = editor_with_shell(80, 30, crate_dir().join("src"));
    poll_until(&mut ed, "the prompt", |ed| {
        !shell_text(ed).trim().is_empty()
    });
    let pane = ed.shell.area().unwrap();
    let mut s = Surface::new(80, 30, ed.theme.ui.text);
    render(&ed, &mut s);
    let header = row(&s, pane.y);
    assert!(header.contains("(C-\\ returns)"), "{header:?}");
}

#[test]
fn the_pane_is_not_drawn_when_the_window_is_too_small() {
    let dir = crate_dir();
    // Room for the views and the pane: shown.
    let mut ed = editor_with_shell(80, 24, dir.clone());
    poll_until(&mut ed, "the prompt", |ed| {
        !shell_text(ed).trim().is_empty()
    });
    // The window shrinks below what the views and the pane need together.
    let (w, h) = (80, MIN_ROWS + VIEWS_MIN_ROWS + 1);
    let (_, region, _) = split_screen(w, h, &ed);
    assert!(region.height < MIN_ROWS + VIEWS_MIN_ROWS);
    ed.resize(region);
    assert_eq!(ed.shell.area(), None);
    assert_eq!(ed.area, region, "the views take the whole region back");
    assert!(
        !ed.shell.has_focus(),
        "no keys go to a pane that is not drawn"
    );
    let mut s = Surface::new(w, h, ed.theme.ui.text);
    render(&ed, &mut s);
    for y in 0..h {
        assert!(
            !row(&s, y).contains(" shell  "),
            "row {y}: {:?}",
            row(&s, y)
        );
    }
    // Keys reach the editor: `i` enters insert mode instead of the shell.
    type_text(&mut ed, "i");
    assert_eq!(ed.mode, maestro_editor::Mode::Insert);
    ed.handle_key(KeyEvent::plain(KeyCode::Esc));
    // Grown back, the pane returns and the keys stay with the editor.
    let (_, region, _) = split_screen(80, 24, &ed);
    ed.resize(region);
    assert!(!ed.shell.has_focus());
    let mut s = Surface::new(80, 24, ed.theme.ui.text);
    render(&ed, &mut s);
    let pane = ed.shell.area().unwrap();
    let header = row(&s, pane.y);
    assert!(header.starts_with(" shell  "), "{header:?}");
    assert!(header.contains("KEYS: EDITOR"), "{header:?}");

    // A window too small from the start: space o says so and starts
    // nothing.
    let mut small = Editor::new(Config::default(), Rect::new(0, 1, 80, 10));
    small.cwd = dir;
    small.shell.program = Some("sh".into());
    type_text(&mut small, " o");
    assert!(!small.shell.open);
    assert!(small.shell.session().is_none());
    let msg = small.status.clone().unwrap().1;
    assert!(msg.contains("too small for the shell pane"), "{msg}");
}

#[test]
fn with_the_agent_panel_open_the_header_does_not_offer_ctrl_backslash() {
    let mut ed = editor_with_shell(120, 30, crate_dir());
    poll_until(&mut ed, "the prompt", |ed| {
        !shell_text(ed).trim().is_empty()
    });
    ed.handle_key(KeyEvent::ctrl('\\'));
    ed.run_command("agent_panel");
    assert!(ed.agents.panel_open());
    let pane = ed.shell.area().unwrap();
    let mut s = Surface::new(120, 30, ed.theme.ui.text);
    render(&ed, &mut s);
    let header = row(&s, pane.y);
    assert!(header.contains("KEYS: AGENT PANEL"), "{header:?}");
    assert!(!header.contains("C-\\"), "{header:?}");
    // The panel keeps C-\; the shell does not take the keys.
    ed.handle_key(KeyEvent::ctrl('\\'));
    assert!(!ed.shell.has_focus());
}

#[test]
fn the_scrolled_back_notice_stays_inside_a_narrow_pane() {
    // The views' region as it is between a sidebar and a file tree on an
    // 80-column screen.
    let region = Rect::new(37, 1, 21, 28);
    let mut ed = Editor::new(Config::default(), region);
    ed.cwd = crate_dir();
    ed.shell.program = Some("sh".into());
    type_text(&mut ed, " o");
    poll_until(&mut ed, "the prompt", |ed| {
        !shell_text(ed).trim().is_empty()
    });
    type_text(
        &mut ed,
        "i=0; while [ $i -lt 60 ]; do echo l$i; i=$((i+1)); done\n",
    );
    poll_until(&mut ed, "the output", |ed| {
        shell_text(ed).lines().any(|l| l.trim_end() == "l59")
    });
    let pane = ed.shell.area().unwrap();
    ed.handle_mouse(maestro_editor::MouseKind::ScrollUp, pane.x + 1, pane.y + 2);
    assert!(ed.shell.session().unwrap().terminal.snapshot().scrolled);
    let mut s = Surface::new(80, 30, ed.theme.ui.text);
    render(&ed, &mut s);
    let body = row(&s, pane.y + 1);
    let left: String = body.chars().take(usize::from(pane.x)).collect();
    assert!(left.trim().is_empty(), "drawn left of the pane: {body:?}");
    assert!(body.contains(" scrolled back"), "{body:?}");
}
