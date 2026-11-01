//! Tests for the sidebar drawing (`sidebar.rs`).

use std::path::PathBuf;

use maestro_config::Config;
use maestro_editor::agents::Status;
use maestro_editor::workspaces::{Project, RecordAgent, Upstream, WorktreeRow};
use maestro_editor::{Editor, KeyEvent, Rect};

use crate::render::{render, sidebar_area, split_screen};
use crate::surface::Surface;

fn row(s: &Surface, y: u16) -> String {
    s.row(y).iter().map(|c| c.symbol.as_str()).collect()
}

fn worktree(path: &str, branch: &str, main: bool) -> WorktreeRow {
    let path = PathBuf::from(path);
    WorktreeRow {
        name: path.file_name().unwrap().to_string_lossy().into_owned(),
        path,
        branch: Some(branch.into()),
        head: Some("0123456789abcdef".into()),
        is_main: main,
        missing: false,
        locked: false,
        dirty: Some(0),
        upstream: None,
        error: None,
    }
}

/// An editor in `/w/alpha` whose last scan found alpha (two worktrees,
/// two agents from saved records) and beta, with the sidebar opened by
/// `space s`.
fn editor(width: u16, height: u16) -> Editor {
    let mut ed = Editor::new(
        Config::default(),
        Rect::new(0, 1, width, height.saturating_sub(2)),
    );
    ed.cwd = PathBuf::from("/w/alpha");
    let mut main = worktree("/w/alpha", "main", true);
    main.dirty = Some(2);
    main.upstream = Some(Upstream {
        name: "origin/main".into(),
        ahead: 1,
        behind: 0,
    });
    let agent_wt = worktree("/w/alpha-worktrees/fix-3", "maestro/fix-3", false);
    let record = |name: &str, status, wt: &str| RecordAgent {
        id: 3,
        name: name.into(),
        task: "fix it".into(),
        status,
        archived: false,
        tests_failed: false,
        worktree: PathBuf::from(wt),
    };
    ed.workspaces.projects = vec![
        Project {
            root: PathBuf::from("/w/alpha"),
            name: "alpha".into(),
            worktrees: vec![main, agent_wt],
            records: vec![
                record("sh-4", Status::Stopped, "/w/alpha-worktrees/old-4"),
                record("sh-3", Status::ReadyForReview, "/w/alpha-worktrees/fix-3"),
            ],
            error: None,
            records_note: None,
        },
        Project {
            root: PathBuf::from("/w/beta"),
            name: "beta".into(),
            worktrees: vec![worktree("/w/beta", "trunk", true)],
            records: Vec::new(),
            error: None,
            records_note: None,
        },
    ];
    ed.workspaces.scanned = true;
    ed.workspaces
        .sidebar
        .expanded
        .insert(PathBuf::from("/w/alpha"));
    ed.handle_key(KeyEvent::char(' '));
    ed.handle_key(KeyEvent::char('s'));
    assert!(ed.workspaces.sidebar.has_focus());
    // The scan `space s` started would replace the data above; it is
    // never applied, because nothing polls.
    let (_, views, _) = split_screen(width, height, &ed);
    ed.resize(views);
    ed
}

#[test]
fn sidebar_lists_agents_by_attention_then_projects_with_facts() {
    let ed = editor(100, 30);
    let mut s = Surface::new(100, 30, ed.theme.ui.text);
    render(&ed, &mut s);
    let rows: Vec<String> = (1..29)
        .map(|y| {
            let r: String = row(&s, y).chars().take(36).collect();
            r.trim_end().to_string()
        })
        .collect();
    let want = [
        " workspaces  in alpha:main",
        " needs you (1)",
        "  ! sh-3  ready for review  alpha (e",
        " idle (1)",
        "  - sh-4  idle  alpha (earlier sessi",
        " projects",
        " v alpha  2 worktrees, current",
        "   @ main  here",
        "     main worktree, 2 changed, ↑1 ↓0",
        "     maestro/fix-3",
        "     fix-3, clean",
        "     ! sh-3 ready for review",
        " > beta  1 worktree",
    ];
    assert_eq!(&rows[..want.len()], want, "{rows:#?}");
    // The divider, then the views.
    assert_eq!(s.cell(36, 5).unwrap().symbol, "│");
    assert_eq!(ed.area.x, 37);
    // The current worktree starts selected, in the picker's selection
    // colors on both its lines; the agent above is not.
    assert_eq!(s.cell(4, 8).unwrap().style, ed.theme.ui.popup_selected);
    assert_eq!(s.cell(6, 9).unwrap().style, ed.theme.ui.popup_selected);
    assert_ne!(s.cell(4, 3).unwrap().style, ed.theme.ui.popup_selected);
    // The focused footer, and the status line names the worktree.
    assert!(row(&s, 28).contains("esc editor"), "{:?}", row(&s, 28));
    let status: String = row(&s, 28).chars().skip(37).collect();
    assert!(status.contains("alpha:main"), "{status:?}");
}

#[test]
fn moving_the_selection_scrolls_it_into_view() {
    let mut ed = editor(80, 10);
    for _ in 0..20 {
        ed.handle_key(KeyEvent::char('j'));
    }
    let mut s = Surface::new(80, 10, ed.theme.ui.text);
    render(&ed, &mut s);
    let shown: Vec<String> = (2..8).map(|y| row(&s, y)).collect();
    assert!(
        shown.iter().any(|r| r.contains("> beta")),
        "the last row is on screen: {shown:#?}"
    );
}

#[test]
fn a_narrow_screen_has_no_sidebar_and_nothing_panics() {
    let ed = editor(20, 10);
    assert_eq!(sidebar_area(20, 10, &ed), None);
    assert_eq!(split_screen(20, 10, &ed).1.x, 0);
    for (w, h) in (1..=60u16).flat_map(|w| (1..=14u16).map(move |h| (w, h))) {
        let ed = editor(w, h);
        let mut s = Surface::new(w, h, ed.theme.ui.text);
        render(&ed, &mut s);
    }
}

#[test]
fn before_the_first_scan_it_says_so() {
    let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 100, 28));
    ed.workspaces.sidebar.open = true;
    let (_, views, _) = split_screen(100, 30, &ed);
    ed.resize(views);
    let mut s = Surface::new(100, 30, ed.theme.ui.text);
    render(&ed, &mut s);
    let text: Vec<String> = (1..29).map(|y| row(&s, y)).collect();
    assert!(text.iter().any(|r| r.contains("no agents")), "{text:#?}");
    assert!(text.iter().any(|r| r.contains("scanning projects…")));
    // Not focused: no footer.
    assert!(!row(&s, 28).contains("esc editor"));
}
