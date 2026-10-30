//! `git status --porcelain=v2 -z`, parsed.

use std::path::{Path, PathBuf};

use crate::{Access, GitError, Repository, Result, bytes_to_path};

/// One side's change to a tracked file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Modified,
    TypeChanged,
    Added,
    Deleted,
    Renamed,
    Copied,
}

impl ChangeKind {
    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            b'M' => ChangeKind::Modified,
            b'T' => ChangeKind::TypeChanged,
            b'A' => ChangeKind::Added,
            b'D' => ChangeKind::Deleted,
            b'R' => ChangeKind::Renamed,
            b'C' => ChangeKind::Copied,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            ChangeKind::Modified => "modified",
            ChangeKind::TypeChanged => "type changed",
            ChangeKind::Added => "added",
            ChangeKind::Deleted => "deleted",
            ChangeKind::Renamed => "renamed",
            ChangeKind::Copied => "copied",
        }
    }
}

/// How a file is in conflict, from the two-letter unmerged codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    BothDeleted,
    AddedByUs,
    DeletedByThem,
    AddedByThem,
    DeletedByUs,
    BothAdded,
    BothModified,
}

impl ConflictKind {
    fn from_codes(xy: &[u8]) -> Option<Self> {
        Some(match xy {
            b"DD" => ConflictKind::BothDeleted,
            b"AU" => ConflictKind::AddedByUs,
            b"UD" => ConflictKind::DeletedByThem,
            b"UA" => ConflictKind::AddedByThem,
            b"DU" => ConflictKind::DeletedByUs,
            b"AA" => ConflictKind::BothAdded,
            b"UU" => ConflictKind::BothModified,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            ConflictKind::BothDeleted => "both deleted",
            ConflictKind::AddedByUs => "added by us",
            ConflictKind::DeletedByThem => "deleted by them",
            ConflictKind::AddedByThem => "added by them",
            ConflictKind::DeletedByUs => "deleted by us",
            ConflictKind::BothAdded => "both added",
            ConflictKind::BothModified => "both modified",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    /// A tracked file with staged and/or unstaged changes.
    Changed {
        staged: Option<ChangeKind>,
        unstaged: Option<ChangeKind>,
    },
    Conflicted(ConflictKind),
    Untracked,
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// Relative to the working tree root.
    pub path: PathBuf,
    /// The source of a rename or copy.
    pub orig_path: Option<PathBuf>,
    pub state: EntryState,
}

impl StatusEntry {
    pub fn staged(&self) -> Option<ChangeKind> {
        match self.state {
            EntryState::Changed { staged, .. } => staged,
            _ => None,
        }
    }

    pub fn unstaged(&self) -> Option<ChangeKind> {
        match self.state {
            EntryState::Changed { unstaged, .. } => unstaged,
            _ => None,
        }
    }

    pub fn is_untracked(&self) -> bool {
        self.state == EntryState::Untracked
    }

    pub fn is_conflicted(&self) -> bool {
        matches!(self.state, EntryState::Conflicted(_))
    }

    /// A short description such as `staged modified, unstaged deleted`.
    pub fn describe(&self) -> String {
        match self.state {
            EntryState::Changed { staged, unstaged } => {
                let mut parts = Vec::new();
                if let Some(s) = staged {
                    parts.push(format!("staged {}", s.label()));
                }
                if let Some(u) = unstaged {
                    parts.push(format!("unstaged {}", u.label()));
                }
                let mut s = parts.join(", ");
                if let Some(o) = &self.orig_path {
                    s.push_str(&format!(" from {}", o.display()));
                }
                s
            }
            EntryState::Conflicted(k) => format!("conflict: {}", k.label()),
            EntryState::Untracked => "untracked".into(),
            EntryState::Ignored => "ignored".into(),
        }
    }
}

/// Branch facts from `# branch.*` headers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchInfo {
    /// The commit id, or `None` before the first commit.
    pub oid: Option<String>,
    /// The branch name, or `None` when `HEAD` is detached.
    pub head: Option<String>,
    pub upstream: Option<String>,
    /// Commits ahead of and behind the upstream (0 without one).
    pub ahead: u32,
    pub behind: u32,
    /// An upstream is configured but its ref no longer exists, so `ahead`
    /// and `behind` are unknown (both 0).
    pub upstream_gone: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub branch: BranchInfo,
    pub entries: Vec<StatusEntry>,
}

impl Status {
    /// No staged, unstaged, conflicted or untracked files. Ignored files do
    /// not count.
    pub fn is_clean(&self) -> bool {
        self.entries.iter().all(|e| e.state == EntryState::Ignored)
    }

    pub fn entry(&self, path: &Path) -> Option<&StatusEntry> {
        self.entries.iter().find(|e| e.path == path)
    }

    pub fn untracked(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.is_untracked())
    }

    pub fn conflicted(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.is_conflicted())
    }

    /// Tracked files with staged or unstaged changes, and conflicts.
    pub fn uncommitted(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| {
            matches!(
                e.state,
                EntryState::Changed { .. } | EntryState::Conflicted(_)
            )
        })
    }
}

impl Repository {
    /// Status of every file, with each untracked file listed (not just its
    /// directory). Read-only: it never takes the index lock.
    pub fn status(&self) -> Result<Status> {
        self.status_with(false)
    }

    /// Like [`Repository::status`], also listing ignored files.
    pub fn status_with(&self, ignored: bool) -> Result<Status> {
        let mut args = vec![
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
            "--renames",
        ];
        if ignored {
            args.push("--ignored=matching");
        }
        let out = self.run(&args, Access::ReadOnly)?;
        parse_status(&out).map_err(|detail| GitError::Parse {
            args: args.join(" "),
            detail,
        })
    }
}

/// Parses `git status --porcelain=v2 -z --branch` output.
pub(crate) fn parse_status(out: &[u8]) -> std::result::Result<Status, String> {
    let mut status = Status::default();
    let mut fields = out.split(|c| *c == 0).filter(|f| !f.is_empty());
    let bad = |rec: &[u8]| format!("bad record {:?}", String::from_utf8_lossy(rec));
    let mut saw_ab = false;
    while let Some(rec) = fields.next() {
        match rec[0] {
            b'#' => {
                let text = String::from_utf8_lossy(rec);
                let mut it = text.splitn(3, ' ');
                let _ = it.next();
                let key = it.next().unwrap_or_default();
                let value = it.next().unwrap_or_default().to_string();
                let b = &mut status.branch;
                match key {
                    "branch.oid" => b.oid = (value != "(initial)").then_some(value),
                    "branch.head" => b.head = (value != "(detached)").then_some(value),
                    "branch.upstream" => b.upstream = Some(value),
                    "branch.ab" => {
                        saw_ab = true;
                        for part in value.split(' ') {
                            if let Some(n) = part.strip_prefix('+') {
                                b.ahead = n.parse().unwrap_or(0);
                            } else if let Some(n) = part.strip_prefix('-') {
                                b.behind = n.parse().unwrap_or(0);
                            }
                        }
                    }
                    _ => {}
                }
            }
            b'1' | b'2' => {
                let n = if rec[0] == b'1' { 9 } else { 10 };
                let parts: Vec<&[u8]> = rec.splitn(n, |c| *c == b' ').collect();
                if parts.len() != n || parts[1].len() != 2 {
                    return Err(bad(rec));
                }
                let xy = parts[1];
                let orig_path = if rec[0] == b'2' {
                    Some(bytes_to_path(fields.next().ok_or_else(|| bad(rec))?))
                } else {
                    None
                };
                status.entries.push(StatusEntry {
                    path: bytes_to_path(parts[n - 1]),
                    orig_path,
                    state: EntryState::Changed {
                        staged: ChangeKind::from_code(xy[0]),
                        unstaged: ChangeKind::from_code(xy[1]),
                    },
                });
            }
            b'u' => {
                let parts: Vec<&[u8]> = rec.splitn(11, |c| *c == b' ').collect();
                if parts.len() != 11 {
                    return Err(bad(rec));
                }
                let kind = ConflictKind::from_codes(parts[1]).ok_or_else(|| bad(rec))?;
                status.entries.push(StatusEntry {
                    path: bytes_to_path(parts[10]),
                    orig_path: None,
                    state: EntryState::Conflicted(kind),
                });
            }
            b'?' | b'!' => {
                let path = rec.get(2..).ok_or_else(|| bad(rec))?;
                status.entries.push(StatusEntry {
                    path: bytes_to_path(path),
                    orig_path: None,
                    state: if rec[0] == b'?' {
                        EntryState::Untracked
                    } else {
                        EntryState::Ignored
                    },
                });
            }
            _ => return Err(bad(rec)),
        }
    }
    status.branch.upstream_gone = status.branch.upstream.is_some() && !saw_ab;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_record_kind() {
        let out = b"# branch.oid 1234abcd\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaaa bbbb with space.txt\0\
2 R. N... 100644 100644 100644 aaaa bbbb R100 new name.txt\0old name.txt\0\
u UU N... 100644 100644 100644 100644 aaaa bbbb cccc both.txt\0\
? new.txt\0! target/x\0";
        let s = parse_status(out).unwrap();
        assert_eq!(s.branch.head.as_deref(), Some("main"));
        assert_eq!(s.branch.upstream.as_deref(), Some("origin/main"));
        assert_eq!((s.branch.ahead, s.branch.behind), (2, 1));
        assert!(!s.branch.upstream_gone);
        assert_eq!(s.entries.len(), 5);
        assert_eq!(s.entries[0].path, PathBuf::from("with space.txt"));
        assert_eq!(s.entries[0].unstaged(), Some(ChangeKind::Modified));
        assert_eq!(s.entries[0].staged(), None);
        assert_eq!(s.entries[1].staged(), Some(ChangeKind::Renamed));
        assert_eq!(s.entries[1].orig_path, Some(PathBuf::from("old name.txt")));
        assert_eq!(
            s.entries[2].state,
            EntryState::Conflicted(ConflictKind::BothModified)
        );
        assert!(s.entries[3].is_untracked());
        assert_eq!(s.entries[4].state, EntryState::Ignored);
        assert!(!s.is_clean());
    }

    #[test]
    fn detached_and_initial_heads() {
        let s = parse_status(b"# branch.oid (initial)\0# branch.head (detached)\0").unwrap();
        assert_eq!(s.branch.oid, None);
        assert_eq!(s.branch.head, None);
        assert!(s.is_clean());
        let gone = parse_status(b"# branch.oid 12\0# branch.head a\0# branch.upstream origin/a\0")
            .unwrap();
        assert!(gone.branch.upstream_gone);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_status(b"1 M\0").is_err());
        assert!(parse_status(b"z what\0").is_err());
    }
}
