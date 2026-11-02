//! Tests for the GitHub inbox drawing (`inbox.rs`). The data comes from
//! maestro-forge's recorded `gh` fixtures; the configured `gh` does not
//! exist, so nothing here runs the real one.

use std::path::Path;
use std::time::{Duration, Instant};

use maestro_config::Config;
use maestro_editor::forge::api::{self, Inbox, Section, SectionKind};
use maestro_editor::{Editor, KeyEvent, Rect};

use crate::surface::Surface;

fn fixture(name: &str) -> Vec<u8> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../maestro-forge/tests/fixtures")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn key(name: &str) -> KeyEvent {
    name.parse().unwrap()
}

fn row(s: &Surface, y: u16) -> String {
    s.row(y).iter().map(|c| c.symbol.as_str()).collect()
}

fn screen(s: &Surface) -> Vec<String> {
    (0..s.height).map(|y| row(s, y)).collect()
}

fn draw(ed: &Editor, w: u16, h: u16) -> Surface {
    let mut s = Surface::new(w, h, ed.theme.ui.text);
    crate::render::render(ed, &mut s);
    s
}

fn settle(ed: &mut Editor) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while ed.has_background_work() {
        ed.poll_background();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    ed.poll_background();
}

/// An inbox from the fixtures: three review requests (two with status, one
/// whose status failed), two empty sections, issues and the repository.
fn inbox() -> Inbox {
    let mut rr = api::parse_search_prs(&fixture("search_prs.json")).unwrap();
    let ok = api::parse_status(&fixture("pr_status_checkruns.json")).unwrap();
    let failing = api::parse_status(&fixture("pr_status_contexts.json")).unwrap();
    rr[0].status = Some(Ok(ok));
    rr[1].status = Some(Ok(failing));
    rr[2].status = Some(Err(api::ForgeError::Network("dial tcp: refused".into())));
    let section = |kind, items| Section {
        kind,
        items: Ok(items),
        repo: Err(String::new()),
        limited: false,
    };
    Inbox {
        fatal: None,
        sections: vec![
            section(SectionKind::ReviewRequested, rr),
            section(SectionKind::AssignedPrs, Vec::new()),
            section(SectionKind::MyPrs, Vec::new()),
            section(
                SectionKind::AssignedIssues,
                api::parse_search_issues(&fixture("search_issues.json")).unwrap(),
            ),
            Section {
                kind: SectionKind::ThisRepository,
                items: Ok(api::parse_pr_list(&fixture("pr_list.json"), "cli/cli").unwrap()),
                repo: Ok("cli/cli".into()),
                limited: false,
            },
        ],
    }
}

fn editor(w: u16, h: u16) -> Editor {
    let mut config = Config::default();
    config.github.gh = "/nonexistent/maestro-tui-test/gh".into();
    Editor::new(config, Rect::new(0, 1, w, h.saturating_sub(2)))
}

/// The inbox open on the fixtures, as if loaded a minute ago.
fn loaded(w: u16, h: u16) -> Editor {
    let mut ed = editor(w, h);
    ed.handle_key(KeyEvent::char(' '));
    ed.handle_key(KeyEvent::char('i'));
    settle(&mut ed);
    ed.forge.inbox = Some(inbox());
    let at = maestro_editor::forge::now_unix() - 60;
    ed.forge.refreshed = Some((at, Instant::now()));
    ed
}

#[test]
fn the_list_shows_sections_rows_checks_and_the_refresh_time() {
    let ed = loaded(150, 40);
    let s = draw(&ed, 150, 40);
    let rows = screen(&s);
    let text = rows.join("\n");
    assert!(rows[1].contains("GitHub inbox"), "{:?}", rows[1]);
    assert!(rows[1].contains("UTC (1m ago)"), "{:?}", rows[1]);
    assert!(rows[1].contains("KEYS: INBOX"), "{:?}", rows[1]);
    assert!(rows[2].contains("Review requested (3)"), "{:?}", rows[2]);
    let first = &rows[3];
    assert!(
        first.starts_with('▌'),
        "the selected row is marked: {first:?}"
    );
    for part in [
        "cli/cli#14846",
        "Add shared team PR preparation skill",
        "tidy-dev",
        "✓ passed 6",
    ] {
        assert!(first.contains(part), "{part} in {first:?}");
    }
    let second = &rows[4];
    assert!(second.contains("✗ failed 3/13"), "{second:?}");
    let third = &rows[5];
    assert!(third.contains("draft"), "{third:?}");
    assert!(third.contains("? status not loaded"), "{third:?}");
    // Empty sections say so.
    assert!(
        text.contains("no open pull requests are assigned to you"),
        "{text}"
    );
    assert!(text.contains("you have no open pull requests"), "{text}");
    assert!(text.contains("Assigned issues (3)"), "{text}");
    assert!(text.contains("issue"), "{text}");
    assert!(text.contains("This repository: cli/cli (3)"), "{text}");
    assert!(text.contains("passed 1, 20 skipped"), "{text}");
    assert!(rows[38].contains("ret details"), "{:?}", rows[38]);

    // The checks word carries the state's color, with its symbol.
    let y = 4;
    let x = row(&s, y).chars().position(|c| c == '✗').unwrap() as u16;
    assert_eq!(
        s.cell(x, y).unwrap().style.fg,
        ed.theme.agent.failed.fg,
        "the failed mark uses the failed color"
    );
    let y = 3;
    let x = row(&s, y).chars().position(|c| c == '✓').unwrap() as u16;
    assert_eq!(s.cell(x, y).unwrap().style.fg, ed.theme.agent.completed.fg);
}

#[test]
fn a_missing_gh_and_a_logged_out_gh_say_what_to_do() {
    let mut ed = editor(120, 20);
    ed.handle_key(KeyEvent::char(' '));
    ed.handle_key(KeyEvent::char('i'));
    let text = screen(&draw(&ed, 120, 20)).join("\n");
    assert!(text.contains("loading the inbox from GitHub"), "{text}");
    settle(&mut ed);
    let text = screen(&draw(&ed, 120, 20)).join("\n");
    assert!(text.contains("the GitHub CLI was not found"), "{text}");
    assert!(
        text.contains("install it from https://cli.github.com"),
        "{text}"
    );

    ed.forge.inbox = Some(Inbox {
        fatal: Some(api::ForgeError::NotLoggedIn),
        sections: Vec::new(),
    });
    let text = screen(&draw(&ed, 120, 20)).join("\n");
    assert!(
        text.contains("gh is not logged in to GitHub; run `gh auth login`"),
        "{text}"
    );
    assert!(text.contains("Run `gh auth login` in a terminal"), "{text}");
}

#[test]
fn the_status_line_shows_review_requests_once_loaded() {
    let mut ed = editor(120, 20);
    let text = screen(&draw(&ed, 120, 20)).join("\n");
    assert!(!text.contains("review request"), "{text}");
    ed.forge.inbox = Some(inbox());
    let rows = screen(&draw(&ed, 120, 20));
    let status = &rows[18];
    assert!(status.contains("3 review requests: space i"), "{status:?}");
}

#[test]
fn the_detail_view_shows_facts_checks_and_the_diff() {
    let mut ed = loaded(150, 40);
    ed.handle_key(key("ret"));
    settle(&mut ed);
    let mut d = api::parse_detail(&fixture("pr_detail.json"), "cli/cli").unwrap();
    d.reference.number = 14846;
    let files = maestro_git_files();
    let v = ed.forge.screen.detail.as_mut().unwrap();
    v.detail = Some(Ok(d));
    v.diff = Some(Ok(files));
    let s = draw(&ed, 150, 40);
    let rows = screen(&s);
    let text = rows.join("\n");
    assert!(rows[1].contains("cli/cli#14846"), "{:?}", rows[1]);
    assert!(rows[1].contains("KEYS: PULL REQUEST"), "{:?}", rows[1]);
    for part in [
        "overview",
        "M .github/workflows/dependabot",
        "chore(deps): bump actions/create-github-app-token from 3.2.0 to 3.3.0",
        "open, by app/dependabot",
        "trunk ← dependabot/github_actions/actions/create-github-app-token-3.3.0",
        "review: approved",
        "merge: no conflicts",
        "checks: ✓ passed 9, 7 skipped",
        "labels: blocked, dependencies, github_actions",
        "Checks: ✓ passed 9, 7 skipped",
        "Reviews (1)",
        "babakks: approved",
        "✓ passed",
        "- skipped",
    ] {
        assert!(text.contains(part), "{part} in\n{text}");
    }
    // The first file's diff, in the review screen's colors.
    ed.handle_key(key("tab"));
    let s = draw(&ed, 150, 40);
    let rows = screen(&s);
    let (y, line) = rows
        .iter()
        .enumerate()
        .find(|(_, r)| r.contains("+        uses: actions/create-github-app-token@64ab"))
        .expect("the added line is drawn");
    let x = line.find("+        uses").unwrap();
    let x = line[..x].chars().count() as u16;
    let bg = s.cell(x + 3, y as u16).unwrap().style.bg;
    assert_eq!(bg, Some(ed.theme.diff.add_bg), "{line:?}");
    assert!(rows.join("\n").contains("@@ -1218,7 +1218,7 @@"));
}

fn maestro_git_files() -> Vec<maestro_editor::agents::DiffFile> {
    api::parse_diff(&fixture("pr_diff.patch")).unwrap()
}

#[test]
fn write_dialogs_show_the_command() {
    let mut ed = loaded(150, 40);
    ed.handle_key(KeyEvent::char('a'));
    let text = screen(&draw(&ed, 150, 40)).join("\n");
    assert!(text.contains("approve cli/cli#14846"), "{text}");
    assert!(
        text.contains("gh api --method POST repos/cli/cli/pulls/14846/reviews --input -"),
        "{text}"
    );
    assert!(
        text.contains(
            r#"{"commit_id":"aaf296326df06cbafd523c12efa0112cdc53c10c","event":"APPROVE"}"#
        ),
        "{text}"
    );
    assert!(text.contains("y send to GitHub"), "{text}");
    ed.handle_key(KeyEvent::char('n'));
    ed.handle_key(KeyEvent::char('m'));
    let text = screen(&draw(&ed, 150, 40)).join("\n");
    assert!(text.contains("merge cli/cli#14846: how?"), "{text}");
    assert!(text.contains("squash and merge"), "{text}");
    ed.handle_key(KeyEvent::char('s'));
    let text = screen(&draw(&ed, 150, 40)).join("\n");
    // The command wraps at the dialog's width; all of it is on screen.
    assert!(
        text.contains("gh pr merge 14846 --repo cli/cli --squash --match-head-commit"),
        "{text}"
    );
    assert!(
        text.contains("aaf296326df06cbafd523c12efa0112cdc53c10c"),
        "{text}"
    );
    assert!(
        text.contains("the reviews the repository requires"),
        "{text}"
    );
}

#[test]
fn never_panics_at_small_sizes() {
    for (w, h) in (1..=60u16).flat_map(|w| (1..=14u16).map(move |h| (w, h))) {
        let mut ed = loaded(w, h);
        let _ = draw(&ed, w, h);
        ed.handle_key(KeyEvent::char('a'));
        let _ = draw(&ed, w, h);
        ed.handle_key(KeyEvent::char('n'));
        ed.handle_key(key("ret"));
        let _ = draw(&ed, w, h);
    }
}
