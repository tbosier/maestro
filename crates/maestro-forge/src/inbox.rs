//! The read-only queries: searches across repositories, one repository's
//! open pull requests, a pull request's status, details and diff, and the
//! whole inbox at once.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use maestro_git::DiffFile;

use crate::model::{Item, PrDetail, PrRef, PrStatus};
use crate::parse;
use crate::{ForgeError, Gh, GhCommand, Result};

/// Results asked for per section.
pub const DEFAULT_LIMIT: usize = 30;
/// Pull request status lookups run at once.
const STATUS_THREADS: usize = 6;

const SEARCH_PR_FIELDS: &str = "number,title,repository,author,isDraft,createdAt,url";
const SEARCH_ISSUE_FIELDS: &str = "number,title,repository,author,createdAt,url";
const STATUS_FIELDS: &str = "number,isDraft,reviewDecision,mergeable,statusCheckRollup,headRefOid";
const LIST_FIELDS: &str = "number,title,author,isDraft,createdAt,url,reviewDecision,mergeable,\
statusCheckRollup,headRefOid";
pub(crate) const DETAIL_FIELDS: &str = "number,title,body,author,state,isDraft,createdAt,url,\
labels,reviews,statusCheckRollup,files,reviewDecision,mergeable,mergeStateStatus,headRefName,\
baseRefName,headRefOid,isCrossRepository";

/// `gh search prs <filter> --state=open`, across repositories. `filter`
/// is one gh flag such as `--review-requested=@me`.
pub fn search_prs(gh: &Gh, filter: &str, limit: usize) -> Result<Vec<Item>> {
    let cmd = GhCommand::new([
        "search".to_string(),
        "prs".into(),
        filter.into(),
        "--state=open".into(),
        "--limit".into(),
        limit.to_string(),
        "--json".into(),
        SEARCH_PR_FIELDS.into(),
    ]);
    gh.json(&cmd, parse::parse_search_prs)
}

/// `gh search issues <filter> --state=open`, across repositories.
pub fn search_issues(gh: &Gh, filter: &str, limit: usize) -> Result<Vec<Item>> {
    let cmd = GhCommand::new([
        "search".to_string(),
        "issues".into(),
        filter.into(),
        "--state=open".into(),
        "--limit".into(),
        limit.to_string(),
        "--json".into(),
        SEARCH_ISSUE_FIELDS.into(),
    ]);
    gh.json(&cmd, parse::parse_search_issues)
}

/// A pull request's review decision, mergeability, checks and head.
pub fn pr_status(gh: &Gh, pr: &PrRef) -> Result<PrStatus> {
    let cmd = GhCommand::new([
        "pr".to_string(),
        "view".into(),
        pr.number.to_string(),
        "--repo".into(),
        pr.repo.clone(),
        "--json".into(),
        STATUS_FIELDS.into(),
    ]);
    gh.json(&cmd, parse::parse_status)
}

/// The `owner/name` (`host/owner/name` off github.com) of the GitHub
/// repository `dir` is in. `Ok(None)` with
/// gh's reason when it is not in one (no Git repository, or no remote on
/// a GitHub host); errors that are not about the directory (gh missing,
/// logged out, network) stay errors.
pub fn current_repo(gh: &Gh, dir: &Path) -> Result<std::result::Result<String, String>> {
    let cmd = GhCommand::new(["repo", "view", "--json", "nameWithOwner,url"]).cwd(dir);
    match gh.json(&cmd, parse::parse_repo_view) {
        Ok(name) => Ok(Ok(name)),
        Err(ForgeError::Command { message, .. }) => Ok(Err(message)),
        Err(e) => Err(e),
    }
}

/// The open pull requests of `repo`, with their status.
pub fn repo_prs(gh: &Gh, repo: &str, limit: usize) -> Result<Vec<Item>> {
    let cmd = GhCommand::new([
        "pr".to_string(),
        "list".into(),
        "--repo".into(),
        repo.into(),
        "--state".into(),
        "open".into(),
        "--limit".into(),
        limit.to_string(),
        "--json".into(),
        LIST_FIELDS.into(),
    ]);
    gh.json(&cmd, |out| parse::parse_pr_list(out, repo))
}

fn pr_view_one(pr: &PrRef, field: &str) -> GhCommand {
    GhCommand::new([
        "pr".to_string(),
        "view".into(),
        pr.number.to_string(),
        "--repo".into(),
        pr.repo.clone(),
        "--json".into(),
        field.into(),
    ])
}

/// The pull request's head commit now.
pub fn pr_head(gh: &Gh, pr: &PrRef) -> Result<String> {
    gh.json(&pr_view_one(pr, "headRefOid"), parse::parse_head)
}

/// The pull request's state now: `OPEN`, `CLOSED` or `MERGED`.
pub fn pr_state(gh: &Gh, pr: &PrRef) -> Result<String> {
    gh.json(&pr_view_one(pr, "state"), parse::parse_state)
}

/// The detail and the diff, checked to describe the same commit: the head
/// is read before and after both, and must be the commit the detail
/// names. If the pull request moved while they loaded, the detail is a
/// [`ForgeError::HeadMoved`] (the diff may be of either commit).
pub fn pr_detail_and_diff(gh: &Gh, pr: &PrRef) -> (Result<PrDetail>, Result<Vec<DiffFile>>) {
    let before = match pr_head(gh, pr) {
        Ok(h) => h,
        Err(e) => return (Err(e.clone()), Err(e)),
    };
    let (detail, diff) = std::thread::scope(|s| {
        let d = s.spawn(|| pr_detail(gh, pr));
        let f = pr_diff(gh, pr);
        let d = d
            .join()
            .unwrap_or_else(|_| Err(ForgeError::Io("the detail thread panicked".into())));
        (d, f)
    });
    let after = pr_head(gh, pr);
    let detail = match (detail, after) {
        (Ok(d), Ok(after)) if d.status.head_oid == before && after == before => Ok(d),
        (Ok(d), Ok(after)) => Err(ForgeError::HeadMoved {
            shown: if d.status.head_oid != before {
                d.status.head_oid
            } else {
                before
            },
            now: after,
        }),
        (Err(e), _) | (_, Err(e)) => Err(e),
    };
    (detail, diff)
}

/// Everything the detail view shows except the diff.
pub fn pr_detail(gh: &Gh, pr: &PrRef) -> Result<PrDetail> {
    let cmd = GhCommand::new([
        "pr".to_string(),
        "view".into(),
        pr.number.to_string(),
        "--repo".into(),
        pr.repo.clone(),
        "--json".into(),
        DETAIL_FIELDS.into(),
    ]);
    gh.json(&cmd, |out| parse::parse_detail(out, &pr.repo))
}

/// The pull request's diff from `gh pr diff`, parsed with maestro-git's
/// unified diff parser.
pub fn pr_diff(gh: &Gh, pr: &PrRef) -> Result<Vec<DiffFile>> {
    let cmd = GhCommand::new([
        "pr".to_string(),
        "diff".into(),
        pr.number.to_string(),
        "--repo".into(),
        pr.repo.clone(),
        "--color".into(),
        "never".into(),
    ]);
    gh.json(&cmd, crate::parse_diff)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionKind {
    ReviewRequested,
    AssignedPrs,
    MyPrs,
    AssignedIssues,
    ThisRepository,
}

impl SectionKind {
    pub const ALL: [SectionKind; 5] = [
        SectionKind::ReviewRequested,
        SectionKind::AssignedPrs,
        SectionKind::MyPrs,
        SectionKind::AssignedIssues,
        SectionKind::ThisRepository,
    ];

    pub fn title(self) -> &'static str {
        match self {
            SectionKind::ReviewRequested => "Review requested",
            SectionKind::AssignedPrs => "Assigned PRs",
            SectionKind::MyPrs => "My PRs",
            SectionKind::AssignedIssues => "Assigned issues",
            SectionKind::ThisRepository => "This repository",
        }
    }

    /// What an empty section says.
    pub fn empty_text(self) -> &'static str {
        match self {
            SectionKind::ReviewRequested => "no open pull requests ask for your review",
            SectionKind::AssignedPrs => "no open pull requests are assigned to you",
            SectionKind::MyPrs => "you have no open pull requests",
            SectionKind::AssignedIssues => "no open issues are assigned to you",
            SectionKind::ThisRepository => "this repository has no open pull requests",
        }
    }
}

/// One section of the inbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub kind: SectionKind,
    pub items: Result<Vec<Item>>,
    /// For "This repository": the repository's name, or why there is no
    /// section (the working directory is not a GitHub repository).
    pub repo: std::result::Result<String, String>,
    /// The search returned as many results as were asked for; there may
    /// be more.
    pub limited: bool,
}

/// The whole inbox, as of one refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbox {
    /// Set when nothing could be loaded for a reason that applies to every
    /// section: gh missing, not logged in, or (when every query failed
    /// with it) the network. The sections then hold nothing to show.
    pub fatal: Option<ForgeError>,
    pub sections: Vec<Section>,
}

impl Inbox {
    pub fn section(&self, kind: SectionKind) -> Option<&Section> {
        self.sections.iter().find(|s| s.kind == kind)
    }

    /// Open pull requests waiting for the user's review, when that search
    /// succeeded.
    pub fn review_requests(&self) -> Option<usize> {
        if self.fatal.is_some() {
            return None;
        }
        self.section(SectionKind::ReviewRequested)
            .and_then(|s| s.items.as_ref().ok())
            .map(Vec::len)
    }

    /// The current repository's `owner/name`, when the working directory
    /// is in one.
    pub fn current_repo(&self) -> Option<&str> {
        self.section(SectionKind::ThisRepository)
            .and_then(|s| s.repo.as_ref().ok())
            .map(String::as_str)
    }
}

/// Loads every section for a user working in `cwd`. The four searches and
/// the repository lookup run at once; then the status of each pull request
/// found by a search (searches do not return it) is fetched, a few at a
/// time, once per pull request.
pub fn load_inbox(gh: &Gh, cwd: &Path, limit: usize) -> Inbox {
    let searches = [
        (SectionKind::ReviewRequested, "--review-requested=@me", true),
        (SectionKind::AssignedPrs, "--assignee=@me", true),
        (SectionKind::MyPrs, "--author=@me", true),
        (SectionKind::AssignedIssues, "--assignee=@me", false),
    ];
    let (mut sections, repo_section) = std::thread::scope(|s| {
        let handles: Vec<_> = searches
            .iter()
            .map(|&(kind, filter, prs)| {
                s.spawn(move || {
                    let items = if prs {
                        search_prs(gh, filter, limit)
                    } else {
                        search_issues(gh, filter, limit)
                    };
                    (kind, items)
                })
            })
            .collect();
        let repo = s.spawn(move || match current_repo(gh, cwd) {
            Ok(Ok(name)) => {
                let items = repo_prs(gh, &name, limit);
                (Ok(name), items)
            }
            Ok(Err(why)) => (Err(why), Ok(Vec::new())),
            Err(e) => (Err(e.to_string()), Err(e)),
        });
        let sections: Vec<Section> = handles
            .into_iter()
            .zip(searches)
            .map(|(h, (kind, _, _))| {
                let items = h
                    .join()
                    .map(|(_, items)| items)
                    .unwrap_or_else(|_| Err(ForgeError::Io("a query thread panicked".into())));
                let limited = items.as_ref().is_ok_and(|v| v.len() >= limit);
                Section {
                    kind,
                    items,
                    repo: Err(String::new()),
                    limited,
                }
            })
            .collect();
        let repo = repo.join().unwrap_or_else(|_| {
            let e = ForgeError::Io("a query thread panicked".into());
            (Err(e.to_string()), Err(e))
        });
        (sections, repo)
    });
    let (repo, items) = repo_section;
    let limited = items.as_ref().is_ok_and(|v| v.len() >= limit);
    sections.push(Section {
        kind: SectionKind::ThisRepository,
        items,
        repo,
        limited,
    });

    let errors: Vec<&ForgeError> = sections
        .iter()
        .filter_map(|s| s.items.as_ref().err())
        .collect();
    let fatal = errors
        .iter()
        .find(|e| e.is_setup())
        .map(|e| (*e).clone())
        .or_else(|| {
            (errors.len() == sections.len()
                && errors.iter().all(|e| matches!(e, ForgeError::Network(_))))
            .then(|| errors[0].clone())
        });
    if let Some(f) = fatal {
        return Inbox {
            fatal: Some(f),
            sections,
        };
    }
    fill_statuses(gh, &mut sections);
    Inbox {
        fatal: None,
        sections,
    }
}

/// Fetches the status of every search result that is a pull request.
fn fill_statuses(gh: &Gh, sections: &mut [Section]) {
    let mut wanted: Vec<PrRef> = sections
        .iter()
        .filter_map(|s| s.items.as_ref().ok())
        .flatten()
        .filter(|i| i.is_pr() && i.status.is_none())
        .map(|i| i.reference.clone())
        .collect();
    wanted.sort();
    wanted.dedup();
    if wanted.is_empty() {
        return;
    }
    let queue = Mutex::new(wanted);
    let done: Mutex<BTreeMap<PrRef, Result<PrStatus>>> = Mutex::new(BTreeMap::new());
    std::thread::scope(|s| {
        for _ in 0..STATUS_THREADS {
            s.spawn(|| {
                // The guard moves into the closure and is dropped there, so
                // the queue is not locked while gh runs.
                while let Some(pr) = queue.lock().ok().and_then(|mut q| q.pop()) {
                    let st = pr_status(gh, &pr);
                    if let Ok(mut d) = done.lock() {
                        d.insert(pr, st);
                    }
                }
            });
        }
    });
    let done = done.into_inner().unwrap_or_default();
    for s in sections.iter_mut() {
        if let Ok(items) = &mut s.items {
            for i in items.iter_mut().filter(|i| i.is_pr() && i.status.is_none()) {
                if let Some(st) = done.get(&i.reference) {
                    if let Ok(st) = st {
                        i.draft = st.draft;
                    }
                    i.status = Some(st.clone());
                }
            }
        }
    }
}
