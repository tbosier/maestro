//! Commit history with a graph, and one commit's details and diff.

use crate::repo::check_rev;
use crate::{Access, FilePatch, GitError, Repository, Result, parse_patches};

/// One row of `git log --graph`: graph characters, and the commit on that
/// row if it has one (rows between commits carry only graph lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// The graph drawing at the start of the row, such as `* ` or `|\`.
    pub graph: String,
    pub commit: Option<LogCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogCommit {
    pub id: String,
    pub short: String,
    pub author: String,
    /// `YYYY-MM-DD`, author date.
    pub date: String,
    /// Branch and tag names pointing here, as `git log --format=%D` gives
    /// them (`HEAD -> main, origin/main`).
    pub refs: String,
    pub subject: String,
}

/// A commit's metadata and its diff against its first parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDetail {
    pub id: String,
    pub parents: Vec<String>,
    /// `Name <email>`.
    pub author: String,
    /// ISO 8601 author date.
    pub date: String,
    pub message: String,
    pub files: Vec<FilePatch>,
}

const SEP: char = '\u{1f}';
const FORMAT: &str = "--format=%x1f%H%x1f%h%x1f%an%x1f%ad%x1f%D%x1f%s";

/// Parses `git log` output made with [`FORMAT`], with or without `--graph`.
pub(crate) fn parse_log(out: &[u8]) -> Vec<LogEntry> {
    String::from_utf8_lossy(out)
        .lines()
        .map(|line| match line.split_once(SEP) {
            None => LogEntry {
                graph: line.trim_end().to_string(),
                commit: None,
            },
            Some((graph, rest)) => {
                let f: Vec<&str> = rest.splitn(6, SEP).collect();
                let get = |i: usize| f.get(i).copied().unwrap_or_default().to_string();
                LogEntry {
                    graph: graph.to_string(),
                    commit: Some(LogCommit {
                        id: get(0),
                        short: get(1),
                        author: get(2),
                        date: get(3),
                        refs: get(4),
                        subject: get(5),
                    }),
                }
            }
        })
        .collect()
}

impl Repository {
    /// Up to `limit` commits reachable from `HEAD`, newest first, with the
    /// graph when `graph` is set. Empty before the first commit.
    pub fn log(&self, limit: usize, graph: bool) -> Result<Vec<LogEntry>> {
        if !self.has_head()? {
            return Ok(Vec::new());
        }
        let n = format!("--max-count={}", limit.max(1));
        let mut args = vec![
            "log",
            "--no-color",
            "--no-show-signature",
            "--date=short",
            FORMAT,
            &n,
        ];
        if graph {
            args.push("--graph");
        }
        args.push("HEAD");
        args.push("--");
        let out = self.run(&args, Access::ReadOnly)?;
        let mut entries = parse_log(&out);
        // A graph row after the last commit (the graph's tail) is noise.
        while entries.last().is_some_and(|e| e.commit.is_none()) {
            entries.pop();
        }
        Ok(entries)
    }

    /// A commit's metadata and its diff against its first parent (the
    /// whole tree for a root commit).
    pub fn commit_detail(&self, rev: &str) -> Result<CommitDetail> {
        check_rev(rev)?;
        let id = self.resolve(rev)?;
        let meta = self.run(
            &[
                "show",
                "-s",
                "--no-color",
                "--no-show-signature",
                "--date=iso-strict",
                "--format=%H%x00%P%x00%an <%ae>%x00%ad%x00%B",
                &id,
                "--",
            ],
            Access::ReadOnly,
        )?;
        let text = String::from_utf8_lossy(&meta);
        let f: Vec<&str> = text.splitn(5, '\0').collect();
        if f.len() != 5 {
            return Err(GitError::Parse {
                args: format!("show -s {id}"),
                detail: text.into_owned(),
            });
        }
        let diff = self.run(
            &[
                "show",
                "--format=",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--no-show-signature",
                "--find-renames",
                "--unified=3",
                "--diff-merges=first-parent",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                &id,
                "--",
            ],
            Access::ReadOnly,
        )?;
        let files = parse_patches(&diff).map_err(|detail| GitError::Parse {
            args: format!("show {id}"),
            detail,
        })?;
        Ok(CommitDetail {
            id: f[0].trim().to_string(),
            parents: f[1].split_whitespace().map(str::to_string).collect(),
            author: f[2].to_string(),
            date: f[3].to_string(),
            message: f[4].trim_end().to_string(),
            files,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_graph_rows_and_commit_rows() {
        let out = "* \u{1f}aaaa\u{1f}aa\u{1f}Ann\u{1f}2026-10-31\u{1f}HEAD -> main\u{1f}Merge x\n\
                   |\\  \n\
                   | * \u{1f}bbbb\u{1f}bb\u{1f}Bo\u{1f}2026-10-30\u{1f}\u{1f}subject with \u{1f} inside\n";
        let e = parse_log(out.as_bytes());
        assert_eq!(e.len(), 3);
        let c = e[0].commit.as_ref().unwrap();
        assert_eq!(e[0].graph, "* ");
        assert_eq!((c.short.as_str(), c.refs.as_str()), ("aa", "HEAD -> main"));
        assert_eq!(e[1].graph, "|\\");
        assert!(e[1].commit.is_none());
        let c = e[2].commit.as_ref().unwrap();
        assert_eq!(e[2].graph, "| * ");
        assert_eq!(c.subject, "subject with \u{1f} inside");
        assert_eq!(c.refs, "");
    }
}
