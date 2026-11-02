//! `git blame --porcelain`, parsed: which commit last changed each line.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use crate::{Access, GitError, Repository, Result};

/// A commit as blame describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameCommit {
    /// Full id; all zeros for lines not committed yet.
    pub id: String,
    pub author: String,
    /// With the angle brackets, as git prints it.
    pub author_mail: String,
    /// Author time, seconds since the Unix epoch.
    pub time: i64,
    /// Author time zone, such as `+0200`.
    pub tz: String,
    pub summary: String,
}

impl BlameCommit {
    /// True for lines that are in the working tree (or the given contents)
    /// but in no commit.
    pub fn uncommitted(&self) -> bool {
        self.id.bytes().all(|b| b == b'0')
    }

    /// The first seven characters of the id.
    pub fn short(&self) -> &str {
        &self.id[..self.id.len().min(7)]
    }

    /// The author date, `YYYY-MM-DD`, in the author's time zone.
    pub fn date(&self) -> String {
        let offset = parse_tz(&self.tz);
        let (y, m, d) = civil_from_days((self.time + offset).div_euclid(86_400));
        format!("{y:04}-{m:02}-{d:02}")
    }
}

/// One line of the blamed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    pub commit: Arc<BlameCommit>,
    /// One-based line number in the commit's version of the file.
    pub orig_line: usize,
    /// One-based line number in the blamed text.
    pub final_line: usize,
}

/// Seconds east of UTC for `+HHMM` / `-HHMM`.
fn parse_tz(tz: &str) -> i64 {
    let (sign, digits) = match tz.as_bytes().first() {
        Some(b'-') => (-1, &tz[1..]),
        Some(b'+') => (1, &tz[1..]),
        _ => (1, tz),
    };
    let n: i64 = digits.parse().unwrap_or(0);
    sign * ((n / 100) * 3600 + (n % 100) * 60)
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parses `git blame --porcelain` output.
pub(crate) fn parse_porcelain(out: &[u8]) -> std::result::Result<Vec<BlameLine>, String> {
    let text = String::from_utf8_lossy(out);
    let mut commits: HashMap<String, BlameCommit> = HashMap::new();
    let mut shared: HashMap<String, Arc<BlameCommit>> = HashMap::new();
    // (id, orig, final) per line, resolved to shared commits at the end.
    let mut rows: Vec<(String, usize, usize)> = Vec::new();
    let mut current: Option<String> = None;
    for line in text.split('\n') {
        if line.is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            // The line's content ends its record.
            current = None;
            continue;
        }
        if current.is_none() {
            let mut parts = line.split(' ');
            let id = parts.next().unwrap_or_default();
            let orig = parts.next().and_then(|n| n.parse().ok());
            let fin = parts.next().and_then(|n| n.parse().ok());
            let (Some(orig), Some(fin)) = (orig, fin) else {
                return Err(format!("bad blame header {line:?}"));
            };
            if id.len() < 7 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("bad blame header {line:?}"));
            }
            commits
                .entry(id.to_string())
                .or_insert_with(|| BlameCommit {
                    id: id.to_string(),
                    author: String::new(),
                    author_mail: String::new(),
                    time: 0,
                    tz: String::new(),
                    summary: String::new(),
                });
            rows.push((id.to_string(), orig, fin));
            current = Some(id.to_string());
            continue;
        }
        let id = current.as_deref().expect("checked above");
        let c = commits.get_mut(id).expect("inserted with the header");
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "author" => c.author = value.to_string(),
            "author-mail" => c.author_mail = value.to_string(),
            "author-time" => c.time = value.parse().unwrap_or(0),
            "author-tz" => c.tz = value.to_string(),
            "summary" => c.summary = value.to_string(),
            _ => {}
        }
    }
    let mut out = Vec::with_capacity(rows.len());
    for (id, orig_line, final_line) in rows {
        let commit = shared
            .entry(id.clone())
            .or_insert_with(|| Arc::new(commits[&id].clone()))
            .clone();
        out.push(BlameLine {
            commit,
            orig_line,
            final_line,
        });
    }
    out.sort_by_key(|l| l.final_line);
    Ok(out)
}

impl Repository {
    /// Blames `path` (relative to the root, or absolute inside it). With
    /// `contents`, blames that text as the file's current version (an
    /// editor buffer with unsaved changes); otherwise the file on disk.
    /// `lines` limits it to one-based lines `from..=to`.
    pub fn blame(
        &self,
        path: &Path,
        contents: Option<&[u8]>,
        lines: Option<(usize, usize)>,
    ) -> Result<Vec<BlameLine>> {
        let rel = self.relative(path).ok_or_else(|| {
            GitError::InvalidArgument(format!(
                "{} is outside {}",
                path.display(),
                self.root().display()
            ))
        })?;
        let mut args: Vec<OsString> = vec!["blame".into(), "--porcelain".into()];
        if let Some((from, to)) = lines {
            args.push("-L".into());
            args.push(format!("{},{}", from.max(1), to.max(from).max(1)).into());
        }
        if contents.is_some() {
            args.push("--contents".into());
            args.push("-".into());
        }
        args.push("--".into());
        args.push(rel.into_os_string());
        let out = match contents {
            Some(c) => self.run_input(&args, c, Access::ReadOnly)?,
            None => self.run(&args, Access::ReadOnly)?,
        };
        parse_porcelain(&out).map_err(|detail| GitError::Parse {
            args: "blame --porcelain".into(),
            detail,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = "\
1111111111111111111111111111111111111111 1 1 2
author Ann Author
author-mail <ann@example.invalid>
author-time 1761955200
author-tz -0500
committer Ann Author
committer-mail <ann@example.invalid>
committer-time 1761955200
committer-tz -0500
summary First commit
boundary
filename f.txt
\tone
1111111111111111111111111111111111111111 2 2
\ttwo
0000000000000000000000000000000000000000 3 3 1
author Not Committed Yet
author-mail <not.committed.yet>
author-time 1762000000
author-tz +0000
summary Version of f.txt from f.txt
filename f.txt
\tsummary looks like a key but is content
";

    #[test]
    fn parses_porcelain_with_repeated_commits() {
        let lines = parse_porcelain(OUT.as_bytes()).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].commit.author, "Ann Author");
        assert_eq!(lines[0].commit.summary, "First commit");
        assert!(Arc::ptr_eq(&lines[0].commit, &lines[1].commit));
        assert_eq!((lines[1].orig_line, lines[1].final_line), (2, 2));
        assert_eq!(lines[0].commit.short(), "1111111");
        // 1761955200 is 2025-11-01 00:00 UTC; at -0500 it is still Oct 31.
        assert_eq!(lines[0].commit.date(), "2025-10-31");
        assert!(lines[2].commit.uncommitted());
        assert_eq!(lines[2].commit.summary, "Version of f.txt from f.txt");
    }

    #[test]
    fn dates_and_garbage() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(parse_tz("+0530"), 19_800);
        assert!(parse_porcelain(b"not a header\n").is_err());
    }
}
