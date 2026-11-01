//! Parsing `gh ... --json` output. Every field gh may leave out or send as
//! `null` is optional here, so a missing value never fails a whole list.

use serde::Deserialize;

use crate::model::{
    Check, FileStat, Item, ItemKind, Mergeable, PrDetail, PrRef, PrStatus, Review, ReviewDecision,
};
use crate::time::parse_time;

#[derive(Deserialize, Default)]
struct Author {
    login: Option<String>,
}

fn login(a: Option<Author>) -> String {
    a.and_then(|a| a.login).unwrap_or_default()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repo {
    name_with_owner: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchItem {
    number: u64,
    title: Option<String>,
    repository: Repo,
    author: Option<Author>,
    is_draft: Option<bool>,
    created_at: Option<String>,
    url: Option<String>,
}

fn json<'a, T: Deserialize<'a>>(out: &'a [u8]) -> Result<T, String> {
    serde_json::from_slice(out).map_err(|e| e.to_string())
}

fn search(out: &[u8], kind: ItemKind) -> Result<Vec<Item>, String> {
    let raw: Vec<SearchItem> = json(out)?;
    Ok(raw
        .into_iter()
        .map(|r| Item {
            kind,
            reference: PrRef::new(r.repository.name_with_owner, r.number),
            title: r.title.unwrap_or_default(),
            author: login(r.author),
            created_at: r.created_at.as_deref().and_then(parse_time),
            url: r.url.unwrap_or_default(),
            draft: r.is_draft.unwrap_or(false),
            status: None,
        })
        .collect())
}

/// `gh search prs --json number,title,repository,author,isDraft,createdAt,url`.
pub fn parse_search_prs(out: &[u8]) -> Result<Vec<Item>, String> {
    search(out, ItemKind::PullRequest)
}

/// `gh search issues --json number,title,repository,author,createdAt,url`.
pub fn parse_search_issues(out: &[u8]) -> Result<Vec<Item>, String> {
    search(out, ItemKind::Issue)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCheck {
    #[serde(rename = "__typename")]
    typename: Option<String>,
    name: Option<String>,
    context: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    state: Option<String>,
    workflow_name: Option<String>,
    details_url: Option<String>,
    target_url: Option<String>,
}

impl RawCheck {
    fn into_check(self) -> Check {
        let context = self.typename.as_deref() == Some("StatusContext");
        let context_state = if context {
            // A status context with no state is still waiting.
            self.state.clone().unwrap_or_else(|| "PENDING".into())
        } else {
            String::new()
        };
        let (state, detail) = Check::state_of(
            self.status.as_deref().unwrap_or(""),
            self.conclusion.as_deref().unwrap_or(""),
            &context_state,
        );
        let name = if context {
            self.context.or(self.name)
        } else {
            self.name.or(self.context)
        };
        Check {
            name: name.unwrap_or_default(),
            workflow: self.workflow_name.unwrap_or_default(),
            state,
            detail,
            url: self.details_url.or(self.target_url).unwrap_or_default(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawStatus {
    is_draft: Option<bool>,
    review_decision: Option<String>,
    mergeable: Option<String>,
    status_check_rollup: Option<Vec<RawCheck>>,
    head_ref_oid: Option<String>,
}

impl RawStatus {
    fn into_status(self) -> PrStatus {
        PrStatus {
            draft: self.is_draft.unwrap_or(false),
            review_decision: ReviewDecision::parse(self.review_decision.as_deref().unwrap_or("")),
            mergeable: Mergeable::parse(self.mergeable.as_deref().unwrap_or("")),
            checks: self
                .status_check_rollup
                .unwrap_or_default()
                .into_iter()
                .map(RawCheck::into_check)
                .collect(),
            head_oid: self.head_ref_oid.unwrap_or_default(),
        }
    }
}

/// `gh pr view --json number,isDraft,reviewDecision,mergeable,statusCheckRollup,headRefOid`.
pub fn parse_status(out: &[u8]) -> Result<PrStatus, String> {
    let raw: RawStatus = json(out)?;
    Ok(raw.into_status())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListItem {
    number: u64,
    title: Option<String>,
    author: Option<Author>,
    created_at: Option<String>,
    url: Option<String>,
    #[serde(flatten)]
    status: RawStatus,
}

/// `gh pr list --json number,title,author,isDraft,createdAt,url,
/// reviewDecision,mergeable,statusCheckRollup,headRefOid` for `repo`.
pub fn parse_pr_list(out: &[u8], repo: &str) -> Result<Vec<Item>, String> {
    let raw: Vec<ListItem> = json(out)?;
    Ok(raw
        .into_iter()
        .map(|r| {
            let status = r.status.into_status();
            Item {
                kind: ItemKind::PullRequest,
                reference: PrRef::new(repo, r.number),
                title: r.title.unwrap_or_default(),
                author: login(r.author),
                created_at: r.created_at.as_deref().and_then(parse_time),
                url: r.url.unwrap_or_default(),
                draft: status.draft,
                status: Some(Ok(status)),
            }
        })
        .collect())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoView {
    name_with_owner: String,
}

/// `gh repo view --json nameWithOwner`.
pub fn parse_repo_view(out: &[u8]) -> Result<String, String> {
    let raw: RepoView = json(out)?;
    if raw.name_with_owner.is_empty() {
        return Err("no repository name".into());
    }
    Ok(raw.name_with_owner)
}

#[derive(Deserialize)]
struct Label {
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReview {
    author: Option<Author>,
    state: Option<String>,
    body: Option<String>,
    submitted_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFile {
    path: Option<String>,
    additions: Option<u64>,
    deletions: Option<u64>,
    change_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDetail {
    number: u64,
    title: Option<String>,
    body: Option<String>,
    author: Option<Author>,
    state: Option<String>,
    created_at: Option<String>,
    url: Option<String>,
    labels: Option<Vec<Label>>,
    reviews: Option<Vec<RawReview>>,
    files: Option<Vec<RawFile>>,
    merge_state_status: Option<String>,
    head_ref_name: Option<String>,
    base_ref_name: Option<String>,
    is_cross_repository: Option<bool>,
    #[serde(flatten)]
    status: RawStatus,
}

/// `gh pr view --json` with [`crate::inbox::DETAIL_FIELDS`] for a pull
/// request of `repo`.
pub fn parse_detail(out: &[u8], repo: &str) -> Result<PrDetail, String> {
    let r: RawDetail = json(out)?;
    Ok(PrDetail {
        reference: PrRef::new(repo, r.number),
        title: r.title.unwrap_or_default(),
        body: r.body.unwrap_or_default(),
        author: login(r.author),
        state: r.state.unwrap_or_default(),
        created_at: r.created_at.as_deref().and_then(parse_time),
        url: r.url.unwrap_or_default(),
        labels: r
            .labels
            .unwrap_or_default()
            .into_iter()
            .filter_map(|l| l.name)
            .collect(),
        reviews: r
            .reviews
            .unwrap_or_default()
            .into_iter()
            .map(|v| Review {
                author: login(v.author),
                state: v.state.unwrap_or_default(),
                body: v.body.unwrap_or_default(),
                submitted_at: v.submitted_at.as_deref().and_then(parse_time),
            })
            .collect(),
        files: r
            .files
            .unwrap_or_default()
            .into_iter()
            .map(|f| FileStat {
                path: f.path.unwrap_or_default(),
                additions: f.additions.unwrap_or(0),
                deletions: f.deletions.unwrap_or(0),
                change: f.change_type.unwrap_or_default(),
            })
            .collect(),
        merge_state: r.merge_state_status.unwrap_or_default(),
        head_ref: r.head_ref_name.unwrap_or_default(),
        base_ref: r.base_ref_name.unwrap_or_default(),
        cross_repo: r.is_cross_repository.unwrap_or(false),
        status: r.status.into_status(),
    })
}

/// HTML elements GitHub renders in descriptions; other `<...>` text (a
/// generic type in prose, say) is kept.
const HTML_TAGS: &[&str] = &[
    "a",
    "b",
    "i",
    "s",
    "u",
    "p",
    "br",
    "hr",
    "em",
    "strong",
    "code",
    "pre",
    "ul",
    "ol",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "div",
    "span",
    "details",
    "summary",
    "blockquote",
    "img",
    "table",
    "thead",
    "tbody",
    "tr",
    "td",
    "th",
    "sub",
    "sup",
    "kbd",
    "picture",
    "source",
    "del",
    "ins",
    "input",
    "dl",
    "dt",
    "dd",
    "g-emoji",
];
const BLOCK_TAGS: &[&str] = &[
    "p",
    "br",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "div",
    "summary",
    "tr",
    "blockquote",
    "pre",
    "ul",
    "ol",
    "table",
    "details",
    "dt",
    "dd",
];

/// A pull request body as plain text: HTML comments and tags removed, the
/// common entities decoded, line endings normalized, runs of blank lines
/// collapsed to one. Markdown stays as written.
pub fn plain_text(body: &str) -> String {
    let body = body.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(body.len());
    let mut rest = body.as_str();
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if let Some(c) = tail.strip_prefix("<!--") {
            rest = match c.find("-->") {
                Some(j) => &c[j + 3..],
                None => "",
            };
            continue;
        }
        let name: String = tail[1..]
            .trim_start_matches('/')
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();
        let close = tail.find('>').filter(|j| *j < 1000);
        match close {
            Some(j) if HTML_TAGS.contains(&name.as_str()) => {
                rest = &tail[j + 1..];
                // Block tags end a line, so text does not run together.
                if BLOCK_TAGS.contains(&name.as_str())
                    && !out.is_empty()
                    && !out.ends_with('\n')
                    && !rest.starts_with('\n')
                {
                    out.push('\n');
                }
            }
            _ => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    let decoded = out
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    let mut lines: Vec<&str> = Vec::new();
    for l in decoded.lines() {
        let l = l.trim_end();
        if l.is_empty() && lines.last().is_none_or(|p| p.is_empty()) {
            continue;
        }
        lines.push(l);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_strips_html_and_keeps_markdown() {
        let body = "Bumps [x](https://e.x) to 2.\r\n<details>\n<summary>Release notes</summary>\n\
            <p>a &amp; b &lt;c&gt;</p>\n<!-- hidden\nnote -->\n\n\n\n- item `<T>`\nx < y";
        assert_eq!(
            plain_text(body),
            "Bumps [x](https://e.x) to 2.\n\nRelease notes\na & b <c>\n\n- item `<T>`\nx < y"
        );
    }

    #[test]
    fn missing_and_null_fields_do_not_fail() {
        let out = br#"[{"number":3,"title":null,"repository":{"nameWithOwner":"a/b"},
            "author":null,"createdAt":"0001-01-01T00:00:00Z"}]"#;
        let items = parse_search_prs(out).unwrap();
        assert_eq!(items[0].reference, PrRef::new("a/b", 3));
        assert_eq!(items[0].author, "");
        assert_eq!(items[0].created_at, None);
        let s = parse_status(br#"{"statusCheckRollup":null,"reviewDecision":""}"#).unwrap();
        assert_eq!(s.review_decision, ReviewDecision::None);
        assert!(s.checks.is_empty());
        assert!(parse_search_prs(b"not json").is_err());
    }
}
