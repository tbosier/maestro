//! What the inbox shows: pull requests and issues, their review and merge
//! state, and their checks.

use std::fmt;

use crate::ForgeError;

/// A pull request or issue: `owner/repo` and its number.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PrRef {
    /// `owner/name`, or `host/owner/name` for a host other than github.com
    /// (the form `gh --repo` takes).
    pub repo: String,
    pub number: u64,
}

impl PrRef {
    pub fn new(repo: impl Into<String>, number: u64) -> Self {
        Self {
            repo: repo.into(),
            number,
        }
    }

    /// `ghe.example.com/owner/name` splits into the host and
    /// `owner/name`; a plain `owner/name` is on gh's default host.
    pub fn host_and_name(&self) -> (Option<&str>, &str) {
        match self.repo.split_once('/') {
            Some((host, rest)) if rest.contains('/') => (Some(host), rest),
            _ => (None, self.repo.as_str()),
        }
    }
}

impl fmt::Display for PrRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    PullRequest,
    Issue,
}

/// One row of the inbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: ItemKind,
    pub reference: PrRef,
    pub title: String,
    /// The author's login; empty when GitHub has none (a deleted account).
    pub author: String,
    /// Seconds since the Unix epoch.
    pub created_at: Option<i64>,
    pub url: String,
    pub draft: bool,
    /// Review, merge and check state, for pull requests. `None` until it
    /// was asked for; an error when asking failed.
    pub status: Option<Result<PrStatus, ForgeError>>,
}

impl Item {
    pub fn is_pr(&self) -> bool {
        self.kind == ItemKind::PullRequest
    }

    /// The status, when it was fetched.
    pub fn pr_status(&self) -> Option<&PrStatus> {
        self.status.as_ref().and_then(|s| s.as_ref().ok())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
    /// The repository does not require reviews, or GitHub gave none.
    None,
}

impl ReviewDecision {
    pub fn parse(s: &str) -> Self {
        match s {
            "APPROVED" => ReviewDecision::Approved,
            "CHANGES_REQUESTED" => ReviewDecision::ChangesRequested,
            "REVIEW_REQUIRED" => ReviewDecision::ReviewRequired,
            _ => ReviewDecision::None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ReviewDecision::Approved => "approved",
            ReviewDecision::ChangesRequested => "changes requested",
            ReviewDecision::ReviewRequired => "review required",
            ReviewDecision::None => "no review decision",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    /// GitHub has not computed it yet, or did not say.
    Unknown,
}

impl Mergeable {
    pub fn parse(s: &str) -> Self {
        match s {
            "MERGEABLE" => Mergeable::Mergeable,
            "CONFLICTING" => Mergeable::Conflicting,
            _ => Mergeable::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mergeable::Mergeable => "no conflicts",
            Mergeable::Conflicting => "has conflicts",
            Mergeable::Unknown => "mergeability not known yet",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Passed,
    Failed,
    Pending,
    /// Neutral or skipped: finished without passing or failing.
    Skipped,
}

impl CheckState {
    pub fn symbol(self) -> &'static str {
        match self {
            CheckState::Passed => "✓",
            CheckState::Failed => "✗",
            CheckState::Pending => "●",
            CheckState::Skipped => "-",
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            CheckState::Passed => "passed",
            CheckState::Failed => "failed",
            CheckState::Pending => "pending",
            CheckState::Skipped => "skipped",
        }
    }
}

/// One entry of GitHub's status check rollup: a check run (Actions and
/// apps) or a commit status context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    /// The Actions workflow, when there is one.
    pub workflow: String,
    pub state: CheckState,
    /// GitHub's own word for it, lowercased (`success`, `in_progress`...).
    pub detail: String,
    pub url: String,
}

impl Check {
    /// Maps a check run's status and conclusion, or a status context's
    /// state, to a [`CheckState`].
    pub fn state_of(status: &str, conclusion: &str, context_state: &str) -> (CheckState, String) {
        if !context_state.is_empty() {
            let s = match context_state {
                "SUCCESS" => CheckState::Passed,
                "FAILURE" | "ERROR" => CheckState::Failed,
                _ => CheckState::Pending,
            };
            return (s, context_state.to_lowercase());
        }
        if status != "COMPLETED" {
            let word = if status.is_empty() { "pending" } else { status };
            return (CheckState::Pending, word.to_lowercase());
        }
        let s = match conclusion {
            "SUCCESS" => CheckState::Passed,
            "NEUTRAL" | "SKIPPED" => CheckState::Skipped,
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
            | "STALE" => CheckState::Failed,
            // Not a conclusion GitHub documents: not counted as passed.
            _ => CheckState::Pending,
        };
        let word = if conclusion.is_empty() {
            "completed"
        } else {
            conclusion
        };
        (s, word.to_lowercase())
    }
}

/// Counts of checks by state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChecksSummary {
    pub passed: usize,
    pub failed: usize,
    pub pending: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overall {
    Passed,
    Failed,
    Pending,
    /// No checks reported.
    None,
}

impl ChecksSummary {
    pub fn of(checks: &[Check]) -> Self {
        let mut s = Self::default();
        for c in checks {
            match c.state {
                CheckState::Passed => s.passed += 1,
                CheckState::Failed => s.failed += 1,
                CheckState::Pending => s.pending += 1,
                CheckState::Skipped => s.skipped += 1,
            }
        }
        s
    }

    pub fn total(&self) -> usize {
        self.passed + self.failed + self.pending + self.skipped
    }

    /// Any failure wins, then anything still running.
    pub fn overall(&self) -> Overall {
        if self.failed > 0 {
            Overall::Failed
        } else if self.pending > 0 {
            Overall::Pending
        } else if self.total() > 0 {
            Overall::Passed
        } else {
            Overall::None
        }
    }

    pub fn symbol(&self) -> &'static str {
        match self.overall() {
            Overall::Passed => CheckState::Passed.symbol(),
            Overall::Failed => CheckState::Failed.symbol(),
            Overall::Pending => CheckState::Pending.symbol(),
            Overall::None => "-",
        }
    }

    /// `failed 1/9`, `pending 2/9`, `passed 9`, `no checks`. Skipped
    /// checks count in the total of a pass.
    pub fn label(&self) -> String {
        let n = self.total();
        match self.overall() {
            Overall::Failed => format!("failed {}/{n}", self.failed),
            Overall::Pending => format!("pending {}/{n}", self.pending),
            Overall::Passed if self.skipped > 0 => {
                format!("passed {}, {} skipped", self.passed, self.skipped)
            }
            Overall::Passed => format!("passed {n}"),
            Overall::None => "no checks".into(),
        }
    }
}

/// A pull request's review, merge and check state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrStatus {
    pub draft: bool,
    pub review_decision: ReviewDecision,
    pub mergeable: Mergeable,
    pub checks: Vec<Check>,
    /// The head commit, for merging only what was looked at.
    pub head_oid: String,
}

impl PrStatus {
    pub fn summary(&self) -> ChecksSummary {
        ChecksSummary::of(&self.checks)
    }
}

/// A submitted review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub author: String,
    /// `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED`...
    pub state: String,
    pub body: String,
    pub submitted_at: Option<i64>,
}

/// A changed file as GitHub lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
    /// GitHub's word: `MODIFIED`, `ADDED`, `DELETED`, `RENAMED`...
    pub change: String,
}

/// Everything the detail view shows about one pull request, except the
/// diff (see [`crate::pr_diff`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrDetail {
    pub reference: PrRef,
    pub title: String,
    /// The description as GitHub stores it (Markdown and HTML).
    pub body: String,
    pub author: String,
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
    pub created_at: Option<i64>,
    pub url: String,
    pub labels: Vec<String>,
    pub reviews: Vec<Review>,
    pub status: PrStatus,
    pub files: Vec<FileStat>,
    /// GitHub's `mergeStateStatus`: `CLEAN`, `BLOCKED`, `DIRTY`...
    pub merge_state: String,
    pub head_ref: String,
    pub base_ref: String,
    /// The head branch is in another repository (a fork).
    pub cross_repo: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(state: CheckState) -> Check {
        Check {
            name: "c".into(),
            workflow: String::new(),
            state,
            detail: String::new(),
            url: String::new(),
        }
    }

    #[test]
    fn summary_labels() {
        use CheckState::*;
        let s = ChecksSummary::of(&[check(Passed), check(Failed), check(Pending)]);
        assert_eq!(
            (s.overall(), s.label()),
            (Overall::Failed, "failed 1/3".into())
        );
        let s = ChecksSummary::of(&[check(Passed), check(Pending)]);
        assert_eq!(s.label(), "pending 1/2");
        let s = ChecksSummary::of(&[check(Passed), check(Skipped)]);
        assert_eq!(s.label(), "passed 1, 1 skipped");
        assert_eq!(ChecksSummary::of(&[]).label(), "no checks");
        assert_eq!(ChecksSummary::of(&[check(Passed)]).symbol(), "✓");
    }

    #[test]
    fn check_states() {
        assert_eq!(
            Check::state_of("COMPLETED", "SUCCESS", "").0,
            CheckState::Passed
        );
        assert_eq!(
            Check::state_of("IN_PROGRESS", "", ""),
            (CheckState::Pending, "in_progress".into())
        );
        assert_eq!(
            Check::state_of("COMPLETED", "TIMED_OUT", "").0,
            CheckState::Failed
        );
        assert_eq!(Check::state_of("", "", "ERROR").0, CheckState::Failed);
        assert_eq!(Check::state_of("", "", "EXPECTED").0, CheckState::Pending);
        assert_eq!(
            Check::state_of("COMPLETED", "SOMETHING_NEW", "").0,
            CheckState::Pending
        );
    }
}
