//! The GitHub inbox through `Editor::handle_key`, against a fake `gh`: a
//! shell script that logs each call's arguments and standard input and
//! answers from maestro-forge's recorded fixtures. No test talks to
//! GitHub; every write action here goes to the fake.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use maestro_config::Config;
use maestro_editor::forge::actions::Dialog;
use maestro_editor::forge::detail::DetailRow;
use maestro_editor::forge::screen::{self, ListRow};
use maestro_editor::forge::{self as forge, api::SectionKind};
use maestro_editor::{Editor, KeyCode, KeyEvent, Mode, Rect};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn keys(script: &str) -> Vec<KeyEvent> {
    let mut out = Vec::new();
    let mut chars = script.chars();
    while let Some(c) = chars.next() {
        if c == '<' {
            let name: String = chars.by_ref().take_while(|&c| c != '>').collect();
            out.push(name.parse().unwrap_or_else(|_| panic!("bad key {name}")));
        } else if c == '\n' {
            out.push(KeyEvent::plain(KeyCode::Enter));
        } else {
            out.push(KeyEvent::char(c));
        }
    }
    out
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../maestro-forge/tests/fixtures")
}

/// Answers every read-only query from the fixtures, and every write with
/// success. `pr checkout` makes the worktree with git, as gh would.
const ANSWERS: &str = r#"
case "$*" in
  "search prs --review-requested=@me"*) cat "$f/search_prs.json" ;;
  "search prs --assignee=@me"*) echo '[]' ;;
  "search prs --author=@me"*) echo '[]' ;;
  "search issues --assignee=@me"*) cat "$f/search_issues.json" ;;
  "repo view"*) cat "$f/repo_view.json" ;;
  "pr list"*) cat "$f/pr_list.json" ;;
  "pr view 14716 --repo cli/cli --json"*) cat "$f/pr_detail.json" ;;
  "pr view 14846 --repo cli/cli --json number,title,body"*)
    sed 's/"number":14716/"number":14846/' "$f/pr_detail.json" ;;
  "pr view"*"--web") exit 0 ;;
  "pr view"*) cat "$f/pr_status_checkruns.json" ;;
  "issue view"*"--web") exit 0 ;;
  "pr diff"*) cat "$f/pr_diff.patch" ;;
  "pr checkout"*) git worktree add -q -b "$7" "$9" >/dev/null 2>&1 || exit 1 ;;
  "pr review"*|"pr comment"*|"pr merge"*) echo done ;;
  *) echo "unexpected: $*" >&2; exit 1 ;;
esac
"#;

struct Fake {
    dir: PathBuf,
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Fake {
    fn new(answers: &str) -> Self {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("maestro-inbox-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("calls")).unwrap();
        std::fs::create_dir_all(dir.join("work")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let script = format!(
            "#!/bin/sh\nd='{}'\nf='{}'\ni=\"$(date +%s%N).$$\"\n\
             for a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$d/calls/$i.args\"\n\
             cat > \"$d/calls/$i.stdin\"\n{answers}\n",
            dir.display(),
            fixtures().display(),
        );
        let gh = dir.join("gh");
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        Fake { dir }
    }

    fn editor(&self) -> Editor {
        let mut config = Config::default();
        config.github.gh = self.dir.join("gh").display().to_string();
        let mut ed = Editor::new(config, Rect::new(0, 0, 160, 40));
        ed.cwd = self.dir.join("work");
        ed
    }

    /// Every call so far, in start order: arguments and standard input.
    fn calls(&self) -> Vec<(Vec<String>, String)> {
        let mut stems: Vec<String> = std::fs::read_dir(self.dir.join("calls"))
            .unwrap()
            .filter_map(|e| {
                let name = e.ok()?.file_name().into_string().ok()?;
                name.strip_suffix(".args").map(String::from)
            })
            .collect();
        stems.sort_by_key(|s| {
            let (t, p) = s.split_once('.').unwrap_or((s, "0"));
            (
                t.parse::<u128>().unwrap_or(0),
                p.parse::<u64>().unwrap_or(0),
            )
        });
        stems
            .iter()
            .map(|s| {
                let read =
                    |ext: &str| std::fs::read_to_string(self.dir.join(format!("calls/{s}.{ext}")));
                (
                    read("args").unwrap().lines().map(String::from).collect(),
                    read("stdin").unwrap_or_default(),
                )
            })
            .collect()
    }

    /// Calls whose arguments start with `prefix`.
    fn calls_to(&self, prefix: &[&str]) -> Vec<(Vec<String>, String)> {
        self.calls()
            .into_iter()
            .filter(|(a, _)| a.len() >= prefix.len() && a[..prefix.len()] == *prefix)
            .collect()
    }

    /// Calls that change something on GitHub.
    fn writes(&self) -> Vec<(Vec<String>, String)> {
        self.calls()
            .into_iter()
            .filter(|(a, _)| {
                a.len() > 1
                    && a[0] == "pr"
                    && ["review", "comment", "merge"].contains(&a[1].as_str())
            })
            .collect()
    }
}

fn settle(ed: &mut Editor) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        ed.poll_background();
        if !ed.has_background_work() {
            ed.poll_background();
            if !ed.has_background_work() {
                return;
            }
        }
        assert!(Instant::now() < deadline, "background work did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn status(ed: &Editor) -> String {
    ed.status.as_ref().map(|s| s.1.clone()).unwrap_or_default()
}

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn open_inbox(ed: &mut Editor) {
    ed.feed_keys(&keys("<space>i"));
    assert!(ed.forge.screen_open());
    settle(ed);
}

fn selected(ed: &Editor) -> String {
    screen::selected_item(ed)
        .map(|(_, it)| it.reference.to_string())
        .unwrap_or_default()
}

#[test]
fn space_i_opens_the_inbox_and_shows_every_section() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    assert_eq!(forge::review_requests(&ed), None);
    ed.feed_keys(&keys("<space>i"));
    assert!(ed.forge.screen_open());
    assert!(ed.forge.loading);
    assert_eq!(
        screen::list_rows(&ed),
        vec![ListRow::Note(screen::NOT_LOADED.into())]
    );
    settle(&mut ed);
    assert!(!ed.forge.loading);
    assert!(ed.forge.refreshed.is_some());
    let headers: Vec<String> = screen::list_rows(&ed)
        .into_iter()
        .filter_map(|r| match r {
            ListRow::Header(_, t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(
        headers,
        [
            "Review requested (3)",
            "Assigned PRs (0)",
            "My PRs (0)",
            "Assigned issues (3)",
            "This repository: cli/cli (3)"
        ]
    );
    // Empty sections say they are empty.
    let rows = screen::list_rows(&ed);
    assert!(rows.contains(&ListRow::Note(
        "no open pull requests are assigned to you".into()
    )));
    assert!(rows.contains(&ListRow::Note("you have no open pull requests".into())));
    assert_eq!(forge::review_requests(&ed), Some(3));
    assert_eq!(selected(&ed), "cli/cli#14846");
    // Esc hides the screen; the data and the badge count stay.
    ed.feed_keys(&keys("<esc>"));
    assert!(!ed.forge.screen_open());
    assert_eq!(forge::review_requests(&ed), Some(3));
}

#[test]
fn the_typed_command_opens_it_too() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    ed.feed_keys(&keys(":inbox\n"));
    assert!(ed.forge.screen_open());
    settle(&mut ed);
    assert_eq!(forge::review_requests(&ed), Some(3));
}

#[test]
fn j_and_k_move_between_items_across_sections() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    open_inbox(&mut ed);
    ed.feed_keys(&keys("jj"));
    assert_eq!(selected(&ed), "cli/cli#14783");
    // Over the empty sections, to the first issue.
    ed.feed_keys(&keys("j"));
    assert_eq!(selected(&ed), "cli/cli#14895");
    let kind = screen::selected_item(&ed).map(|(k, _)| k);
    assert_eq!(kind, Some(SectionKind::AssignedIssues));
    ed.feed_keys(&keys("k"));
    assert_eq!(selected(&ed), "cli/cli#14783");
    ed.feed_keys(&keys("G"));
    let (kind, it) = screen::selected_item(&ed).unwrap();
    assert_eq!(
        (kind, it.reference.to_string()),
        (SectionKind::ThisRepository, "cli/cli#14783".into())
    );
    ed.feed_keys(&keys("g"));
    assert_eq!(selected(&ed), "cli/cli#14846");
}

#[test]
fn r_refreshes_and_keeps_the_selection() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    open_inbox(&mut ed);
    ed.feed_keys(&keys("j"));
    let searches = f.calls_to(&["search", "prs"]).len();
    ed.feed_keys(&keys("r"));
    assert!(ed.forge.loading);
    settle(&mut ed);
    assert_eq!(f.calls_to(&["search", "prs"]).len(), searches * 2);
    assert_eq!(selected(&ed), "cli/cli#14845");
}

#[test]
fn enter_opens_the_detail_with_checks_reviews_and_the_diff() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    open_inbox(&mut ed);
    // The last row: This repository, cli/cli#14783. Up two is #14846,
    // whose detail the fake answers with the recorded one.
    ed.feed_keys(&keys("G"));
    ed.feed_keys(&keys("kk"));
    ed.feed_keys(&keys("\n"));
    let v = ed.forge.screen.detail.as_ref().unwrap();
    assert_eq!(v.pr.to_string(), "cli/cli#14846");
    assert!(v.loading);
    settle(&mut ed);
    let v = ed.forge.screen.detail.as_ref().unwrap();
    assert!(!v.loading);
    assert_eq!(v.detail().unwrap().reviews[0].author, "babakks");
    assert_eq!(v.files().len(), 2);
    let rows = maestro_editor::forge::detail::rows(&ed);
    assert_eq!(
        rows[0],
        DetailRow::Heading("Checks: ✓ passed 9, 7 skipped".into())
    );
    assert!(rows.contains(&DetailRow::Heading("Reviews (1)".into())));
    assert!(rows.contains(&DetailRow::Heading("Description".into())));
    assert_eq!(
        rows.iter()
            .filter(|r| matches!(r, DetailRow::Check(_)))
            .count(),
        16
    );
    // Tab: the first file's diff, in review rows.
    ed.feed_keys(&keys("<tab>"));
    let rows = maestro_editor::forge::detail::rows(&ed);
    assert!(
        matches!(&rows[0], DetailRow::Diff(maestro_editor::agents::review::Row::Hunk { header, .. }) if header.starts_with("@@ -1218,7 +1218,7 @@"))
    );
    ed.feed_keys(&keys("]"));
    let v = ed.forge.screen.detail.as_ref().unwrap();
    assert!(v.cursor > 0, "] moves to the next hunk");
    ed.feed_keys(&keys("s"));
    assert!(ed.forge.screen.detail.as_ref().unwrap().side_by_side);
    ed.feed_keys(&keys("<tab><tab>"));
    assert_eq!(ed.forge.screen.detail.as_ref().unwrap().page, 0);
    // Esc goes back to the list, then closes the screen.
    ed.feed_keys(&keys("<esc>"));
    assert!(ed.forge.screen.detail.is_none());
    assert!(ed.forge.screen_open());
    ed.feed_keys(&keys("<esc>"));
    assert!(!ed.forge.screen_open());
}

#[test]
fn enter_on_an_issue_points_to_the_browser() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    open_inbox(&mut ed);
    ed.feed_keys(&keys("jjj\n"));
    assert!(ed.forge.screen.detail.is_none());
    assert!(status(&ed).contains("is an issue"), "{}", status(&ed));
    ed.feed_keys(&keys("o"));
    settle(&mut ed);
    assert_eq!(
        f.calls_to(&["issue", "view"])[0].0,
        args(&["issue", "view", "14895", "--repo", "cli/cli", "--web"])
    );
    // Write actions refuse an issue.
    ed.feed_keys(&keys("a"));
    assert!(ed.forge.screen.dialog.is_none());
    assert!(status(&ed).contains("is an issue"));
}

#[test]
fn o_opens_the_pull_request_in_the_browser() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    open_inbox(&mut ed);
    ed.feed_keys(&keys("o"));
    settle(&mut ed);
    assert_eq!(
        f.calls_to(&["pr", "view", "14846", "--repo", "cli/cli", "--web"])
            .len(),
        1
    );
    assert_eq!(status(&ed), "opened cli/cli#14846 in the browser");
}

#[test]
fn a_shows_the_exact_command_and_only_y_approves() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    open_inbox(&mut ed);
    ed.feed_keys(&keys("a"));
    let Some(Dialog::Confirm { lines, .. }) = &ed.forge.screen.dialog else {
        panic!("no dialog: {}", status(&ed));
    };
    assert!(
        lines.contains(&"  gh pr review 14846 --repo cli/cli --approve".to_string()),
        "{lines:?}"
    );
    // n cancels: nothing is sent.
    ed.feed_keys(&keys("n"));
    assert!(ed.forge.screen.dialog.is_none());
    assert!(status(&ed).contains("canceled, nothing was sent"));
    // Other keys leave the dialog up.
    ed.feed_keys(&keys("ax"));
    assert!(ed.forge.screen.dialog.is_some());
    ed.feed_keys(&keys("<esc>"));
    settle(&mut ed);
    assert!(f.writes().is_empty());
    ed.feed_keys(&keys("ay"));
    settle(&mut ed);
    assert_eq!(
        f.writes(),
        vec![(
            args(&["pr", "review", "14846", "--repo", "cli/cli", "--approve"]),
            String::new()
        )]
    );
    assert_eq!(status(&ed), "approved cli/cli#14846");
}

#[test]
fn c_writes_a_comment_in_a_buffer_and_pr_send_sends_what_was_shown() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    open_inbox(&mut ed);
    ed.feed_keys(&keys("j"));
    ed.feed_keys(&keys("c"));
    assert!(!ed.forge.screen_open());
    assert_eq!(ed.mode, Mode::Insert);
    assert!(status(&ed).contains(":pr-send"), "{}", status(&ed));
    ed.feed_keys(&keys("Looks right.\nOne nit: 'quotes' and $HOME<esc>"));
    ed.feed_keys(&keys(":pr-send\n"));
    assert!(ed.forge.screen_open());
    let Some(Dialog::Confirm { lines, .. }) = &ed.forge.screen.dialog else {
        panic!("no dialog: {}", status(&ed));
    };
    assert!(lines.contains(&"  gh pr comment 14845 --repo cli/cli --body-file -".to_string()));
    assert!(lines.contains(&"with this text on its standard input (2 lines):".to_string()));
    assert!(lines.contains(&"  │ Looks right.".to_string()));
    assert!(lines.contains(&"  │ One nit: 'quotes' and $HOME".to_string()));
    let draft = ed.forge.drafts[0].doc;
    ed.feed_keys(&keys("y"));
    settle(&mut ed);
    assert_eq!(
        f.writes(),
        vec![(
            args(&[
                "pr",
                "comment",
                "14845",
                "--repo",
                "cli/cli",
                "--body-file",
                "-"
            ]),
            "Looks right.\nOne nit: 'quotes' and $HOME".to_string()
        )]
    );
    assert_eq!(status(&ed), "commented on cli/cli#14845");
    // The draft buffer is gone once its text is on GitHub.
    assert!(!ed.buffers.contains_key(&draft));
    assert!(ed.forge.drafts.is_empty());
}

#[test]
fn request_changes_needs_text_and_a_failed_send_keeps_the_buffer() {
    let failing = ANSWERS.replace(
        r#""pr review"*|"pr comment"*|"pr merge"*) echo done ;;"#,
        r#""pr review"*) echo 'GraphQL: Review cannot be requested from pull request author.' >&2; exit 1 ;;"#,
    );
    let f = Fake::new(&failing);
    let mut ed = f.editor();
    open_inbox(&mut ed);
    ed.feed_keys(&keys("R<esc>:pr-send\n"));
    assert!(ed.forge.screen.dialog.is_none());
    assert!(status(&ed).contains("is empty"), "{}", status(&ed));
    ed.feed_keys(&keys("iPlease add a test.<esc>:pr-send\n"));
    let Some(Dialog::Confirm { lines, .. }) = &ed.forge.screen.dialog else {
        panic!("no dialog: {}", status(&ed));
    };
    assert!(lines.contains(
        &"  gh pr review 14846 --repo cli/cli --request-changes --body-file -".to_string()
    ));
    let draft = ed.forge.drafts[0].doc;
    ed.feed_keys(&keys("y"));
    settle(&mut ed);
    let w = f.calls_to(&["pr", "review"]);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].1, "Please add a test.");
    let s = status(&ed);
    assert!(
        s.starts_with("could not request changes on cli/cli#14846: gh pr review failed: GraphQL")
            && s.ends_with("the message buffer is kept"),
        "{s}"
    );
    assert!(ed.buffers.contains_key(&draft));
    // Pressing R again returns to the same buffer.
    ed.feed_keys(&keys("<space>iR"));
    assert_eq!(ed.view().doc, draft);
}

#[test]
fn pr_send_outside_a_message_buffer_says_so() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    ed.feed_keys(&keys(":pr-send\n"));
    assert!(
        status(&ed).contains("no message to send"),
        "{}",
        status(&ed)
    );
}

#[test]
fn m_chooses_a_method_and_warns_before_merging() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    open_inbox(&mut ed);
    // This repository's draft pull request.
    ed.feed_keys(&keys("G"));
    assert_eq!(selected(&ed), "cli/cli#14783");
    ed.feed_keys(&keys("m"));
    assert!(matches!(ed.forge.screen.dialog, Some(Dialog::Merge { .. })));
    // Esc cancels the choice.
    ed.feed_keys(&keys("<esc>"));
    assert!(ed.forge.screen.dialog.is_none());
    ed.feed_keys(&keys("mj\n"));
    let Some(Dialog::Confirm { lines, .. }) = &ed.forge.screen.dialog else {
        panic!("no confirm: {}", status(&ed));
    };
    let text = lines.join("\n");
    assert!(text.contains("by squash and merge"), "{text}");
    assert!(
        text.contains("WARNING: this pull request is a draft"),
        "{text}"
    );
    assert!(
        text.contains("WARNING: the reviews the repository requires are not done yet"),
        "{text}"
    );
    assert!(text.contains("  gh pr merge 14783 --repo cli/cli --squash --match-head-commit "));
    ed.feed_keys(&keys("n"));
    settle(&mut ed);
    assert!(f.writes().is_empty());
    // A letter picks the method directly; y merges.
    ed.feed_keys(&keys("mry"));
    settle(&mut ed);
    let w = f.writes();
    assert_eq!(w.len(), 1);
    assert_eq!(
        &w[0].0[..6],
        &args(&["pr", "merge", "14783", "--repo", "cli/cli", "--rebase"])
    );
    assert_eq!(w[0].0[6], "--match-head-commit");
    assert_eq!(w[0].0[7].len(), 40);
    assert_eq!(status(&ed), "merged cli/cli#14783");
}

#[test]
fn merge_warnings_name_failing_and_running_checks() {
    let failing = ANSWERS.replace(
        r#""pr view"*) cat "$f/pr_status_checkruns.json" ;;"#,
        r#""pr view"*) cat "$f/pr_status_contexts.json" ;;"#,
    );
    let f = Fake::new(&failing);
    let mut ed = f.editor();
    open_inbox(&mut ed);
    ed.feed_keys(&keys("m"));
    ed.feed_keys(&keys("m"));
    let Some(Dialog::Confirm { lines, .. }) = &ed.forge.screen.dialog else {
        panic!("no confirm: {}", status(&ed));
    };
    let text = lines.join("\n");
    assert!(
        text.contains(
            "WARNING: 3 of 13 checks failed: pull-kubernetes-e2e-gce, \
             pull-kubernetes-integration, pull-kubernetes-linter-hints."
        ),
        "{text}"
    );
    assert!(
        text.contains("WARNING: 1 of 13 checks have not finished: tide."),
        "{text}"
    );
    assert!(text.contains("--merge --match-head-commit 968946c94ee03b1ce0f854f4781c36511e36db0c"));
    ed.feed_keys(&keys("<esc>"));
    assert!(f.writes().is_empty());
}

#[test]
fn a_tiny_window_cannot_confirm_a_dialog_it_cannot_show() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    open_inbox(&mut ed);
    ed.feed_keys(&keys("a"));
    ed.resize(Rect::new(0, 0, 40, 7));
    ed.feed_keys(&keys("y"));
    settle(&mut ed);
    assert!(f.writes().is_empty());
    assert!(status(&ed).contains("too small"), "{}", status(&ed));
    assert!(ed.forge.screen.dialog.is_some());
}

fn git(dir: &Path, a: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"])
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(a)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {a:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn w_checks_the_pull_request_out_next_to_the_repository_and_opens_a_file() {
    let f = Fake::new(ANSWERS);
    let repo = f.dir.join("work");
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let mut ed = f.editor();
    open_inbox(&mut ed);
    ed.feed_keys(&keys("w"));
    settle(&mut ed);
    let wt = f.dir.join("work-pr-14846");
    let call = &f.calls_to(&["pr", "checkout"])[0].0;
    assert_eq!(
        call,
        &args(&[
            "pr",
            "checkout",
            "14846",
            "--repo",
            "cli/cli",
            "--branch",
            "pr-14846",
            "--worktree",
            &wt.display().to_string()
        ])
    );
    assert!(!ed.forge.screen_open());
    assert_eq!(ed.doc().path(), Some(wt.join("README.md").as_path()));
    assert!(
        status(&ed).contains("checked out on branch pr-14846"),
        "{}",
        status(&ed)
    );
    // Again: the worktree is there already; it is opened, not recreated.
    ed.feed_keys(&keys("<space>i"));
    settle(&mut ed);
    ed.feed_keys(&keys("w"));
    settle(&mut ed);
    assert_eq!(f.calls_to(&["pr", "checkout"]).len(), 1);
    assert!(
        status(&ed).contains("already checked out"),
        "{}",
        status(&ed)
    );
}

#[test]
fn w_refuses_outside_a_github_repository() {
    let f = Fake::new(&ANSWERS.replace(
        r#""repo view"*) cat "$f/repo_view.json" ;;"#,
        r#""repo view"*) echo 'failed to run git: fatal: not a git repository' >&2; exit 1 ;;"#,
    ));
    let mut ed = f.editor();
    open_inbox(&mut ed);
    let rows = screen::list_rows(&ed);
    assert!(
        rows.contains(&ListRow::Note(
            "the working directory is not in a GitHub repository (failed to run git: fatal: not a \
         git repository)"
                .into()
        ))
    );
    ed.feed_keys(&keys("w"));
    settle(&mut ed);
    assert!(f.calls_to(&["pr", "checkout"]).is_empty());
    assert!(
        status(&ed).contains("not a GitHub repository"),
        "{}",
        status(&ed)
    );
}

#[test]
fn logged_out_says_how_to_log_in() {
    let f =
        Fake::new("echo 'To get started with GitHub CLI, please run:  gh auth login' >&2; exit 4");
    let mut ed = f.editor();
    open_inbox(&mut ed);
    let rows = screen::list_rows(&ed);
    assert_eq!(
        rows[0],
        ListRow::Error(
            "gh is not logged in to GitHub; run `gh auth login` in a terminal, then refresh".into()
        )
    );
    assert!(matches!(&rows[1], ListRow::Note(n) if n.contains("gh auth login")));
    assert_eq!(forge::review_requests(&ed), None);
    // Nothing to act on.
    ed.feed_keys(&keys("a"));
    assert!(ed.forge.screen.dialog.is_none());
}

#[test]
fn a_missing_gh_says_so() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    ed.config.github.gh = f.dir.join("not-there").display().to_string();
    open_inbox(&mut ed);
    let rows = screen::list_rows(&ed);
    assert!(
        matches!(&rows[0], ListRow::Error(e) if e.starts_with("the GitHub CLI was not found")),
        "{rows:?}"
    );
}

#[test]
fn the_interval_refreshes_in_the_background() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    ed.tick(Instant::now());
    assert!(!ed.forge.loading, "off by default");
    ed.config.github.refresh_secs = 60;
    ed.tick(Instant::now());
    assert!(ed.forge.loading);
    assert!(!ed.forge.screen_open());
    settle(&mut ed);
    assert_eq!(forge::review_requests(&ed), Some(3));
    let n = f.calls().len();
    ed.tick(Instant::now());
    assert!(!ed.forge.loading, "not due yet");
    ed.tick(Instant::now() + Duration::from_secs(61));
    assert!(ed.forge.loading);
    settle(&mut ed);
    assert!(f.calls().len() > n);
}

#[test]
fn a_replayed_macro_cannot_approve() {
    let f = Fake::new(ANSWERS);
    let mut ed = f.editor();
    // The user records a macro while approving by hand.
    ed.feed_keys(&keys("Q"));
    open_inbox(&mut ed);
    ed.feed_keys(&keys("ay"));
    settle(&mut ed);
    ed.feed_keys(&keys("<esc>Q"));
    assert_eq!(f.writes().len(), 1);
    // Replaying it opens the inbox, but its `a` and `y` are ignored.
    ed.feed_keys(&keys("q"));
    settle(&mut ed);
    assert!(ed.forge.screen_open());
    assert!(
        status(&ed).contains("replayed keys are ignored"),
        "{}",
        status(&ed)
    );
    assert!(ed.forge.screen.dialog.is_none());
    assert_eq!(f.writes().len(), 1);
}
