//! Runs the crate against a fake `gh`: a shell script that records each
//! call's arguments and standard input and answers from the recorded
//! fixtures. No test here talks to GitHub, and every write action is
//! checked against the fake only.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use maestro_forge::*;

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fake {
    dir: PathBuf,
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

impl Fake {
    /// A fake gh whose answers come from `body`, a shell `case` on the
    /// joined arguments (`$*`). Every call is logged first.
    fn new(body: &str) -> Self {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("maestro-fakegh-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("calls")).unwrap();
        let script = format!(
            r#"#!/bin/sh
d='{dir}'
f='{fix}'
i="$(date +%s%N).$$"
for a in "$@"; do printf '%s\n' "$a"; done > "$d/calls/$i.args"
cat > "$d/calls/$i.stdin"
pwd > "$d/calls/$i.cwd"
{body}
"#,
            dir = dir.display(),
            fix = fixtures().display(),
        );
        let gh = dir.join("gh");
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        Fake { dir }
    }

    fn gh(&self) -> Gh {
        Gh::new(self.dir.join("gh"))
    }

    /// Each call's arguments and standard input, in the order they started.
    fn calls(&self) -> Vec<(Vec<String>, String)> {
        self.call_files()
            .into_iter()
            .map(|stem| {
                let read = |ext: &str| {
                    std::fs::read_to_string(self.dir.join(format!("calls/{stem}.{ext}"))).unwrap()
                };
                (
                    read("args").lines().map(String::from).collect(),
                    read("stdin"),
                )
            })
            .collect()
    }

    /// Calls to `gh api`.
    fn calls_to_api(&self) -> Vec<(Vec<String>, String)> {
        self.calls()
            .into_iter()
            .filter(|(a, _)| a[0] == "api")
            .collect()
    }

    /// The working directory of call `i`.
    fn cwd(&self, i: usize) -> PathBuf {
        let stem = &self.call_files()[i];
        let cwd = std::fs::read_to_string(self.dir.join(format!("calls/{stem}.cwd"))).unwrap();
        PathBuf::from(cwd.trim())
    }

    fn call_files(&self) -> Vec<String> {
        let mut stems: Vec<String> = std::fs::read_dir(self.dir.join("calls"))
            .unwrap()
            .filter_map(|e| {
                let name = e.ok()?.file_name().into_string().ok()?;
                name.strip_suffix(".args").map(String::from)
            })
            .collect();
        // `<nanoseconds>.<pid>`: numeric order is start order.
        stems.sort_by_key(|s| {
            let (t, p) = s.split_once('.').unwrap_or((s, "0"));
            (
                t.parse::<u128>().unwrap_or(0),
                p.parse::<u64>().unwrap_or(0),
            )
        });
        stems
    }
}

const PR: fn() -> PrRef = || PrRef::new("octo/demo", 41);

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Answers a head check with commit `h1` and every other call with success.
const HEAD_H1: &str = r#"case "$*" in
  "pr view"*"--json headRefOid") echo '{"headRefOid":"h1"}' ;;
  "pr view"*"--json state") echo '{"state":"MERGED"}' ;;
  *) echo ok ;;
esac"#;

#[test]
fn approve_checks_the_head_then_posts_a_review_on_that_commit() {
    let f = Fake::new(HEAD_H1);
    let out = WriteAction::Approve { head: "h1".into() }
        .run(&f.gh(), &PR())
        .unwrap();
    assert_eq!(out, Outcome::Done);
    let calls = f.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].0,
        args(&[
            "pr",
            "view",
            "41",
            "--repo",
            "octo/demo",
            "--json",
            "headRefOid"
        ])
    );
    assert_eq!(
        calls[1].0,
        args(&[
            "api",
            "--method",
            "POST",
            "repos/octo/demo/pulls/41/reviews",
            "--input",
            "-"
        ])
    );
    assert_eq!(calls[1].1, r#"{"commit_id":"h1","event":"APPROVE"}"#);
}

#[test]
fn comment_and_request_changes_send_the_text_on_stdin() {
    let f = Fake::new(HEAD_H1);
    let body = "Two lines,\n--and a leading dash 'quoted' $HOME";
    WriteAction::Comment { body: body.into() }
        .run(&f.gh(), &PR())
        .unwrap();
    WriteAction::RequestChanges {
        body: "please fix".into(),
        head: "h1".into(),
    }
    .run(&f.gh(), &PR())
    .unwrap();
    let calls = f.calls();
    assert_eq!(
        calls[0].0,
        args(&[
            "pr",
            "comment",
            "41",
            "--repo",
            "octo/demo",
            "--body-file",
            "-"
        ])
    );
    assert_eq!(calls[0].1, body);
    let api = f.calls_to_api();
    assert_eq!(api.len(), 1);
    assert_eq!(
        api[0].1,
        r#"{"body":"please fix","commit_id":"h1","event":"REQUEST_CHANGES"}"#
    );
}

#[test]
fn merge_uses_the_method_pins_the_head_and_reads_the_state() {
    let f = Fake::new(HEAD_H1);
    for m in MergeMethod::ALL {
        let out = WriteAction::Merge {
            method: m,
            head: "0123abc".into(),
        }
        .run(&f.gh(), &PR())
        .unwrap();
        assert_eq!(out, Outcome::MergeState(Ok("MERGED".into())));
    }
    let merges: Vec<_> = f
        .calls()
        .into_iter()
        .filter(|(a, _)| a[1] == "merge")
        .collect();
    for (c, flag) in merges.iter().zip(["--merge", "--squash", "--rebase"]) {
        assert_eq!(
            c.0,
            args(&[
                "pr",
                "merge",
                "41",
                "--repo",
                "octo/demo",
                flag,
                "--match-head-commit",
                "0123abc"
            ])
        );
    }
}

#[test]
fn a_refused_write_reports_gh_words() {
    let f = Fake::new(
        r#"case "$*" in
  "pr view"*) echo '{"headRefOid":"h1"}' ;;
  *) echo 'gh: Can not approve your own pull request (HTTP 422)' >&2; exit 1 ;;
esac"#,
    );
    let e = WriteAction::Approve { head: "h1".into() }
        .run(&f.gh(), &PR())
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "gh api failed: gh: Can not approve your own pull request (HTTP 422)"
    );
}

#[test]
fn browse_and_checkout_commands_run_where_asked() {
    let f = Fake::new("exit 0");
    let repo = f.dir.clone();
    f.gh().run(&browse_command(&PR(), true)).unwrap();
    f.gh()
        .run(&checkout_command(&PR(), "pr-41", &repo.join("wt"), &repo))
        .unwrap();
    let calls = f.calls();
    assert_eq!(
        calls[0].0,
        args(&["pr", "view", "41", "--repo", "octo/demo", "--web"])
    );
    let wt = repo.join("wt").display().to_string();
    assert_eq!(
        calls[1].0,
        args(&[
            "pr",
            "checkout",
            "41",
            "--repo",
            "octo/demo",
            "--branch",
            "pr-41",
            "--worktree",
            &wt
        ])
    );
    assert_eq!(
        std::fs::canonicalize(f.cwd(1)).unwrap(),
        std::fs::canonicalize(&repo).unwrap()
    );
}

#[test]
fn a_slow_gh_is_stopped_at_the_timeout() {
    let f = Fake::new("sleep 5");
    let start = Instant::now();
    let e = f
        .gh()
        .timeout(Duration::from_millis(300))
        .run(&GhCommand::new(["search", "prs"]))
        .unwrap_err();
    assert_eq!(e, ForgeError::Timeout { secs: 1 });
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "{:?}",
        start.elapsed()
    );
}

/// Answers every read-only query the inbox makes from the fixtures.
const INBOX: &str = r#"
case "$*" in
  "search prs --review-requested=@me"*) cat "$f/search_prs.json" ;;
  "search prs --assignee=@me"*) echo '[]' ;;
  "search prs --author=@me"*) echo '[]' ;;
  "search issues --assignee=@me"*) cat "$f/search_issues.json" ;;
  "repo view"*) cat "$f/repo_view.json" ;;
  "pr list"*) cat "$f/pr_list.json" ;;
  "pr view 14783"*) echo 'GraphQL: Something went wrong' >&2; exit 1 ;;
  "pr view"*) cat "$f/pr_status_checkruns.json" ;;
  "pr diff"*) cat "$f/pr_diff.patch" ;;
  *) echo "unexpected: $*" >&2; exit 1 ;;
esac
"#;

#[test]
fn inbox_loads_every_section_and_each_status_once() {
    let f = Fake::new(INBOX);
    let inbox = load_inbox(&f.gh(), &f.dir, 30);
    assert_eq!(inbox.fatal, None);
    assert_eq!(inbox.review_requests(), Some(3));
    assert_eq!(inbox.current_repo(), Some("cli/cli"));
    let rr = inbox.section(SectionKind::ReviewRequested).unwrap();
    let items = rr.items.as_ref().unwrap();
    assert_eq!(items[0].pr_status().unwrap().summary().label(), "passed 6");
    // One status lookup failed: that row says so, the rest still load.
    assert!(matches!(
        &items[2].status,
        Some(Err(ForgeError::Command { .. }))
    ));
    assert!(!rr.limited);
    assert_eq!(
        inbox
            .section(SectionKind::AssignedPrs)
            .unwrap()
            .items
            .as_ref()
            .unwrap()
            .len(),
        0
    );
    let issues = inbox.section(SectionKind::AssignedIssues).unwrap();
    assert_eq!(issues.items.as_ref().unwrap().len(), 3);
    let this = inbox.section(SectionKind::ThisRepository).unwrap();
    assert_eq!(this.repo.as_deref(), Ok("cli/cli"));
    assert_eq!(this.items.as_ref().unwrap().len(), 3);
    // The repository list carries status inline: only the three search
    // results needed a `pr view`, one each.
    let views = f
        .calls()
        .iter()
        .filter(|(a, _)| a[0] == "pr" && a[1] == "view")
        .count();
    assert_eq!(views, 3);
    let search: Vec<_> = f
        .calls()
        .into_iter()
        .filter(|(a, _)| a[..2] == ["search", "prs"] && a[2] == "--review-requested=@me")
        .collect();
    assert_eq!(
        search[0].0,
        args(&[
            "search",
            "prs",
            "--review-requested=@me",
            "--state=open",
            "--limit",
            "30",
            "--json",
            "number,title,repository,author,isDraft,createdAt,url"
        ])
    );
}

#[test]
fn a_full_section_says_there_may_be_more() {
    let f = Fake::new(INBOX);
    let inbox = load_inbox(&f.gh(), &f.dir, 3);
    assert!(inbox.section(SectionKind::ReviewRequested).unwrap().limited);
    assert!(!inbox.section(SectionKind::AssignedPrs).unwrap().limited);
}

#[test]
fn outside_a_github_repository_the_section_says_why() {
    let f = Fake::new(
        r#"case "$*" in
  "repo view"*) echo 'failed to run git: fatal: not a git repository (or any parent up to mount point /)' >&2; exit 1 ;;
  *) echo '[]' ;;
esac"#,
    );
    let inbox = load_inbox(&f.gh(), &f.dir, 30);
    assert_eq!(inbox.fatal, None);
    let this = inbox.section(SectionKind::ThisRepository).unwrap();
    assert_eq!(
        this.repo,
        Err(
            "failed to run git: fatal: not a git repository (or any parent up to mount point /)"
                .into()
        )
    );
    assert_eq!(this.items, Ok(Vec::new()));
    assert_eq!(inbox.review_requests(), Some(0));
}

#[test]
fn logged_out_missing_and_offline_are_whole_inbox_problems() {
    // gh's real message and exit code when no account is logged in.
    let f =
        Fake::new("echo 'To get started with GitHub CLI, please run:  gh auth login' >&2; exit 4");
    let inbox = load_inbox(&f.gh(), &f.dir, 30);
    assert_eq!(inbox.fatal, Some(ForgeError::NotLoggedIn));
    assert_eq!(inbox.review_requests(), None);
    assert!(
        ForgeError::NotLoggedIn
            .to_string()
            .contains("run `gh auth login`")
    );

    let missing = Gh::new(f.dir.join("no-such-gh"));
    let inbox = load_inbox(&missing, &f.dir, 30);
    assert!(matches!(inbox.fatal, Some(ForgeError::NotInstalled { .. })));

    let f = Fake::new(
        "echo 'error connecting to api.github.com' >&2; \
         echo 'check your internet connection or https://githubstatus.com' >&2; exit 1",
    );
    let inbox = load_inbox(&f.gh(), &f.dir, 30);
    assert!(
        matches!(inbox.fatal, Some(ForgeError::Network(_))),
        "{inbox:?}"
    );
}

#[test]
fn a_rate_limited_section_fails_alone() {
    let f = Fake::new(
        r#"case "$*" in
  "search issues"*) echo 'HTTP 403: API rate limit exceeded for user ID 1.' >&2; exit 1 ;;
  "repo view"*) cat "$f/repo_view.json" ;;
  *) echo '[]' ;;
esac"#,
    );
    let inbox = load_inbox(&f.gh(), &f.dir, 30);
    assert_eq!(inbox.fatal, None);
    let issues = inbox.section(SectionKind::AssignedIssues).unwrap();
    assert!(matches!(&issues.items, Err(ForgeError::RateLimited(_))));
    assert!(
        issues
            .items
            .as_ref()
            .unwrap_err()
            .to_string()
            .starts_with("GitHub's rate limit was reached")
    );
    assert_eq!(inbox.review_requests(), Some(0));
}

#[test]
fn detail_and_diff() {
    let f = Fake::new(
        r#"case "$*" in
  "pr view"*) cat "$f/pr_detail.json" ;;
  "pr diff"*) cat "$f/pr_diff.patch" ;;
esac"#,
    );
    let pr = PrRef::new("cli/cli", 14716);
    let d = pr_detail(&f.gh(), &pr).unwrap();
    assert_eq!(d.reviews[0].author, "babakks");
    let files = pr_diff(&f.gh(), &pr).unwrap();
    assert_eq!(files.len(), 2);
    let calls = f.calls();
    assert_eq!(
        calls[1].0,
        args(&[
            "pr", "diff", "14716", "--repo", "cli/cli", "--color", "never"
        ])
    );
}

#[test]
fn unparsable_output_is_a_parse_error() {
    let f = Fake::new("echo '<html>'");
    let e = search_prs(&f.gh(), "--author=@me", 5).unwrap_err();
    assert!(
        matches!(e, ForgeError::Parse { ref command, .. } if command == "gh search prs"),
        "{e}"
    );
}

#[test]
fn detail_and_diff_must_describe_one_head() {
    // The head reads A, the detail says A, then the head reads B: a push
    // happened while loading, so the detail is refused.
    let f = Fake::new(
        r#"case "$*" in
  "pr view 14716 --repo cli/cli --json headRefOid")
    if [ -e "$d/seen" ]; then echo '{"headRefOid":"b"}'; else touch "$d/seen"; echo '{"headRefOid":"936921fa9e67cdbf78fb25abc6252afabafef056"}'; fi ;;
  "pr view"*) cat "$f/pr_detail.json" ;;
  "pr diff"*) cat "$f/pr_diff.patch" ;;
esac"#,
    );
    let pr = PrRef::new("cli/cli", 14716);
    let (d, diff) = pr_detail_and_diff(&f.gh(), &pr);
    assert!(
        matches!(d, Err(ForgeError::HeadMoved { ref now, .. }) if now == "b"),
        "{d:?}"
    );
    assert!(diff.is_ok());
    // The same answers without the push: accepted.
    let f = Fake::new(
        r#"case "$*" in
  "pr view 14716 --repo cli/cli --json headRefOid") echo '{"headRefOid":"936921fa9e67cdbf78fb25abc6252afabafef056"}' ;;
  "pr view"*) cat "$f/pr_detail.json" ;;
  "pr diff"*) cat "$f/pr_diff.patch" ;;
esac"#,
    );
    let (d, _) = pr_detail_and_diff(&f.gh(), &pr);
    assert_eq!(d.unwrap().reference.number, 14716);
}

#[test]
fn a_review_is_not_sent_when_the_head_moved() {
    let f = Fake::new(
        r#"case "$*" in
  "pr view"*) echo '{"headRefOid":"new"}' ;;
  *) echo sent ;;
esac"#,
    );
    let e = WriteAction::Approve { head: "old".into() }
        .run(&f.gh(), &PR())
        .unwrap_err();
    assert_eq!(
        e,
        ForgeError::HeadMoved {
            shown: "old".into(),
            now: "new".into()
        }
    );
    assert!(f.calls_to_api().is_empty());
}

#[test]
fn a_merge_reports_the_state_github_gives_afterward() {
    let f = Fake::new(
        r#"case "$*" in
  "pr view"*) echo '{"state":"OPEN"}' ;;
  *) exit 0 ;;
esac"#,
    );
    let out = WriteAction::Merge {
        method: MergeMethod::Merge,
        head: "h".into(),
    }
    .run(&f.gh(), &PR())
    .unwrap();
    assert_eq!(out, Outcome::MergeState(Ok("OPEN".into())));
}
