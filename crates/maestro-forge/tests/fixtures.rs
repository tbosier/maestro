//! Parsing real `gh --json` output. The fixtures were recorded on
//! November 1, 2026 with gh 2.102.1 from read-only commands against public
//! repositories, unedited:
//!
//! - `search_prs.json`: `gh search prs --repo=cli/cli --state=open --limit 3
//!   --json number,title,repository,author,isDraft,createdAt,url`
//! - `search_issues.json`: `gh search issues --repo=cli/cli --state=open
//!   --limit 3 --json number,title,repository,author,createdAt,url`
//! - `pr_status_checkruns.json`: `gh pr view 14846 -R cli/cli --json
//!   number,isDraft,reviewDecision,mergeable,statusCheckRollup,headRefOid`
//! - `pr_status_contexts.json`: the same for `143677 -R
//!   kubernetes/kubernetes` (status contexts rather than check runs)
//! - `pr_list.json`: `gh pr list -R cli/cli --state open --limit 3 --json
//!   number,title,author,isDraft,createdAt,url,reviewDecision,mergeable,
//!   statusCheckRollup,headRefOid`
//! - `pr_detail.json`: `gh pr view 14716 -R cli/cli --json` with the
//!   detail fields in `src/inbox.rs`
//! - `pr_diff.patch`: `gh pr diff 14716 -R cli/cli --color=never`
//! - `repo_view.json`: `gh repo view cli/cli --json nameWithOwner,url`

use maestro_forge::*;

fn fixture(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn search_prs() {
    let items = parse_search_prs(&fixture("search_prs.json")).unwrap();
    assert_eq!(items.len(), 3);
    let a = &items[0];
    assert_eq!(a.kind, ItemKind::PullRequest);
    assert_eq!(a.reference, PrRef::new("cli/cli", 14846));
    assert_eq!(a.reference.to_string(), "cli/cli#14846");
    assert_eq!(a.title, "Add shared team PR preparation skill");
    assert_eq!(a.author, "tidy-dev");
    assert_eq!(a.created_at, parse_time("2026-10-28T02:05:57Z"));
    assert_eq!(a.url, "https://github.com/cli/cli/pull/14846");
    assert!(!a.draft);
    // Searches do not carry the status; it is fetched separately.
    assert!(a.status.is_none());
    assert!(items[2].draft);
}

#[test]
fn search_issues() {
    let items = parse_search_issues(&fixture("search_issues.json")).unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].kind, ItemKind::Issue);
    assert_eq!(items[0].reference, PrRef::new("cli/cli", 14895));
    assert_eq!(items[0].author, "Izzur");
    assert_eq!(
        items[1].title,
        "`gh pr checks` exits 1 for \"no checks reported\", the same code as failed checks"
    );
}

#[test]
fn status_with_check_runs() {
    let s = parse_status(&fixture("pr_status_checkruns.json")).unwrap();
    assert_eq!(s.review_decision, ReviewDecision::ReviewRequired);
    assert_eq!(s.mergeable, Mergeable::Mergeable);
    assert!(!s.draft);
    assert_eq!(s.head_oid, "aaf296326df06cbafd523c12efa0112cdc53c10c");
    assert_eq!(s.checks.len(), 6);
    assert_eq!(s.checks[0].name, "build (ubuntu-latest)");
    assert_eq!(s.checks[0].state, CheckState::Passed);
    assert_eq!(s.checks[0].detail, "success");
    let sum = s.summary();
    assert_eq!((sum.passed, sum.failed, sum.pending), (6, 0, 0));
    assert_eq!((sum.symbol(), sum.label()), ("✓", "passed 6".to_string()));
}

#[test]
fn status_with_status_contexts() {
    let s = parse_status(&fixture("pr_status_contexts.json")).unwrap();
    // kubernetes does not use GitHub's required reviews.
    assert_eq!(s.review_decision, ReviewDecision::None);
    assert_eq!(s.checks.len(), 13);
    assert_eq!(s.checks[0].name, "pull-kubernetes-e2e-gce");
    assert_eq!(s.checks[0].state, CheckState::Failed);
    assert_eq!(s.checks[3].name, "tide");
    assert_eq!(s.checks[3].state, CheckState::Pending);
    assert!(s.checks[0].url.starts_with("https://prow.k8s.io/"));
    let sum = s.summary();
    assert_eq!((sum.passed, sum.failed, sum.pending), (9, 3, 1));
    assert_eq!(sum.overall(), Overall::Failed);
    assert_eq!(sum.label(), "failed 3/13");
}

#[test]
fn repository_list_has_status_inline() {
    let items = parse_pr_list(&fixture("pr_list.json"), "cli/cli").unwrap();
    assert_eq!(items.len(), 3);
    let st = items[0].pr_status().unwrap();
    assert_eq!(st.summary().label(), "passed 6");
    assert_eq!(items[0].author, "tidy-dev");
    let draft = &items[2];
    assert!(draft.draft);
    assert_eq!(
        draft.pr_status().unwrap().summary().label(),
        "passed 1, 20 skipped"
    );
}

#[test]
fn detail() {
    let d = parse_detail(&fixture("pr_detail.json"), "cli/cli").unwrap();
    assert_eq!(d.reference, PrRef::new("cli/cli", 14716));
    assert_eq!(
        d.title,
        "chore(deps): bump actions/create-github-app-token from 3.2.0 to 3.3.0"
    );
    assert_eq!(d.author, "app/dependabot");
    assert_eq!(d.state, "OPEN");
    assert_eq!(d.labels, ["blocked", "dependencies", "github_actions"]);
    assert_eq!(d.reviews.len(), 1);
    assert_eq!(d.reviews[0].author, "babakks");
    assert_eq!(d.reviews[0].state, "APPROVED");
    assert_eq!(d.status.review_decision, ReviewDecision::Approved);
    assert_eq!(
        d.status.head_oid,
        "936921fa9e67cdbf78fb25abc6252afabafef056"
    );
    assert_eq!(d.merge_state, "CLEAN");
    assert_eq!(d.base_ref, "trunk");
    assert!(!d.cross_repo);
    assert_eq!(d.files.len(), 2);
    assert_eq!(
        d.files[0].path,
        ".github/workflows/dependabot-triage.lock.yml"
    );
    assert_eq!((d.files[0].additions, d.files[0].deletions), (2, 2));
    let sum = d.status.summary();
    assert_eq!((sum.passed, sum.skipped), (9, 7));
    let text = plain_text(&d.body);
    assert!(
        text.starts_with("Bumps [actions/create-github-app-token]"),
        "{text}"
    );
    assert!(
        !text.contains("<details>") && !text.contains("<p>"),
        "{text}"
    );
}

#[test]
fn diff_parses_with_the_git_parser() {
    let files = maestro_git::parse_unified_diff(&fixture("pr_diff.patch")).unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(
        files[0].path().unwrap().to_str(),
        Some(".github/workflows/dependabot-triage.lock.yml")
    );
    assert_eq!(files[0].counts(), (2, 2));
    assert_eq!(files[0].hunks[0].old_start, 1218);
}

#[test]
fn repo_view() {
    assert_eq!(
        parse_repo_view(&fixture("repo_view.json")).unwrap(),
        "cli/cli"
    );
}
