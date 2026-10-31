//! A unified diff model (files, hunks, numbered lines) parsed from
//! `git diff`, for review views.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::repo::check_rev;
use crate::{Access, GitError, Repository, Result, bytes_to_path};

/// Which two trees to compare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSpec {
    /// Working tree against the index (`git diff`).
    Unstaged,
    /// Index against `HEAD` (`git diff --cached`).
    Staged,
    /// Working tree against `HEAD` (`git diff HEAD`).
    AgainstHead,
    /// Between two revisions (`git diff from to`).
    Revisions { from: String, to: String },
    /// What `head` changed since it forked from `base`
    /// (`git diff base...head`): the changes a merge of `head` would bring.
    SinceForkPoint { base: String, head: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Added,
    Deleted,
    Modified,
    /// Renamed, with git's similarity percentage.
    Renamed {
        similarity: u8,
    },
    Copied {
        similarity: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// One-based line number in the old file (context and removed lines).
    pub old_line: Option<usize>,
    /// One-based line number in the new file (context and added lines).
    pub new_line: Option<usize>,
    /// The line without its terminator.
    pub text: String,
    /// The line has no newline at the end of the file.
    pub no_newline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    /// The text after the second `@@`, usually an enclosing function.
    pub section: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    /// `None` for an added file.
    pub old_path: Option<PathBuf>,
    /// `None` for a deleted file.
    pub new_path: Option<PathBuf>,
    pub change: FileChange,
    pub old_mode: Option<String>,
    pub new_mode: Option<String>,
    pub binary: bool,
    /// An unmerged file, shown by git as a combined diff (`diff --cc`).
    /// Its hunks are not parsed; `hunks` is empty.
    pub unmerged: bool,
    pub hunks: Vec<Hunk>,
}

impl DiffFile {
    /// The path to show: the new one, or the old one for a deletion.
    pub fn path(&self) -> Option<&PathBuf> {
        self.new_path.as_ref().or(self.old_path.as_ref())
    }

    /// Lines added and removed in this file.
    pub fn counts(&self) -> (usize, usize) {
        let mut added = 0;
        let mut removed = 0;
        for l in self.hunks.iter().flat_map(|h| &h.lines) {
            match l.kind {
                DiffLineKind::Added => added += 1,
                DiffLineKind::Removed => removed += 1,
                DiffLineKind::Context => {}
            }
        }
        (added, removed)
    }
}

impl Repository {
    /// A unified diff with three lines of context, with renames detected.
    /// `paths` limits it to those files (relative to the root); empty means
    /// every file.
    pub fn diff(&self, spec: &DiffSpec, paths: &[PathBuf]) -> Result<Vec<DiffFile>> {
        let mut args: Vec<OsString> = [
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            "--unified=3",
            "--src-prefix=a/",
            "--dst-prefix=b/",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        match spec {
            DiffSpec::Unstaged => {}
            DiffSpec::Staged => args.push("--cached".into()),
            DiffSpec::AgainstHead => args.push("HEAD".into()),
            DiffSpec::Revisions { from, to } => {
                check_rev(from)?;
                check_rev(to)?;
                args.push(from.into());
                args.push(to.into());
            }
            DiffSpec::SinceForkPoint { base, head } => {
                check_rev(base)?;
                check_rev(head)?;
                args.push(format!("{base}...{head}").into());
            }
        }
        args.push("--".into());
        args.extend(paths.iter().map(|p| p.clone().into_os_string()));
        let out = self.run(&args, Access::ReadOnly)?;
        parse_unified(&out).map_err(|detail| GitError::Parse {
            args: "diff".into(),
            detail,
        })
    }
}

/// Undoes git's C-style quoting of unusual path names.
fn unquote(s: &[u8]) -> Vec<u8> {
    if s.len() < 2 || s[0] != b'"' || s[s.len() - 1] != b'"' {
        return s.to_vec();
    }
    let inner = &s[1..s.len() - 1];
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        let c = inner[i];
        if c != b'\\' || i + 1 >= inner.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let e = inner[i + 1];
        i += 2;
        out.push(match e {
            b'a' => 7,
            b'b' => 8,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 11,
            b'f' => 12,
            b'r' => b'\r',
            b'0'..=b'7' => {
                let mut v = u32::from(e - b'0');
                for _ in 0..2 {
                    if let Some(d) = inner.get(i).filter(|d| (b'0'..=b'7').contains(d)) {
                        v = v * 8 + u32::from(d - b'0');
                        i += 1;
                    }
                }
                v as u8
            }
            other => other,
        });
    }
    out
}

/// A path from a `---`/`+++`/`rename` line: unquoted, prefix removed, and
/// `None` for `/dev/null`.
fn header_path(raw: &[u8], prefix: &[u8]) -> Option<PathBuf> {
    let raw = raw.strip_suffix(b"\t").unwrap_or(raw);
    let p = unquote(raw);
    if p == b"/dev/null" {
        return None;
    }
    let p = p.strip_prefix(prefix).unwrap_or(&p);
    Some(bytes_to_path(p))
}

/// Paths from `diff --git a/X b/Y` when no other header names them. Only
/// unambiguous when both are the same, which holds for mode changes,
/// binary files and empty new or deleted files.
fn git_line_paths(rest: &[u8]) -> Option<(PathBuf, PathBuf)> {
    if rest.first() == Some(&b'"') {
        // Two quoted names, or a quoted and a plain one. The closing quote
        // is the first one not escaped by a backslash.
        let mut i = 1;
        while i < rest.len() && rest[i] != b'"' {
            i += if rest[i] == b'\\' { 2 } else { 1 };
        }
        if i >= rest.len() {
            return None;
        }
        let end = i + 1;
        let a = header_path(&rest[..end], b"a/")?;
        let b = header_path(rest.get(end + 1..)?, b"b/")?;
        return Some((a, b));
    }
    let n = rest.len();
    if n < 5 || !(n - 5).is_multiple_of(2) {
        return None;
    }
    let half = (n - 5) / 2;
    let (a, b) = (&rest[2..2 + half], &rest[half + 5..]);
    (rest.starts_with(b"a/") && rest[2 + half..].starts_with(b" b/") && a == b)
        .then(|| (bytes_to_path(a), bytes_to_path(b)))
}

fn parse_range(s: &str) -> Option<(usize, usize)> {
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// Parses `git diff` output with `a/` and `b/` prefixes.
pub(crate) fn parse_unified(out: &[u8]) -> std::result::Result<Vec<DiffFile>, String> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut lines = out.split(|c| *c == b'\n').peekable();
    let bad = |l: &[u8]| format!("unexpected line {:?}", String::from_utf8_lossy(l));
    while let Some(line) = lines.next() {
        if line.is_empty() {
            continue;
        }
        if let Some(p) = line
            .strip_prefix(b"diff --cc ")
            .or_else(|| line.strip_prefix(b"diff --combined "))
        {
            let path = header_path(p, b"");
            files.push(DiffFile {
                old_path: path.clone(),
                new_path: path,
                change: FileChange::Modified,
                old_mode: None,
                new_mode: None,
                binary: false,
                unmerged: true,
                hunks: Vec::new(),
            });
            while lines.peek().is_some_and(|l| {
                !l.starts_with(b"diff --git ")
                    && !l.starts_with(b"diff --cc ")
                    && !l.starts_with(b"diff --combined ")
            }) {
                lines.next();
            }
            continue;
        }
        let Some(rest) = line.strip_prefix(b"diff --git ") else {
            return Err(bad(line));
        };
        let fallback = git_line_paths(rest);
        let mut f = DiffFile {
            old_path: fallback.as_ref().map(|p| p.0.clone()),
            new_path: fallback.map(|p| p.1),
            change: FileChange::Modified,
            old_mode: None,
            new_mode: None,
            binary: false,
            unmerged: false,
            hunks: Vec::new(),
        };
        let mut copied = false;
        // Extended headers.
        while let Some(&h) = lines.peek() {
            if h.starts_with(b"diff --git ") || h.starts_with(b"@@ ") {
                break;
            }
            lines.next();
            let text = String::from_utf8_lossy(h);
            if let Some(m) = text.strip_prefix("old mode ") {
                f.old_mode = Some(m.to_string());
            } else if let Some(m) = text.strip_prefix("new mode ") {
                f.new_mode = Some(m.to_string());
            } else if let Some(m) = text.strip_prefix("new file mode ") {
                f.new_mode = Some(m.to_string());
                f.change = FileChange::Added;
                f.old_path = None;
            } else if let Some(m) = text.strip_prefix("deleted file mode ") {
                f.old_mode = Some(m.to_string());
                f.change = FileChange::Deleted;
                f.new_path = None;
            } else if let Some(p) = h.strip_prefix(b"rename from ") {
                f.old_path = header_path(p, b"");
            } else if let Some(p) = h.strip_prefix(b"rename to ") {
                f.new_path = header_path(p, b"");
            } else if let Some(p) = h.strip_prefix(b"copy from ") {
                copied = true;
                f.old_path = header_path(p, b"");
            } else if let Some(p) = h.strip_prefix(b"copy to ") {
                f.new_path = header_path(p, b"");
            } else if let Some(s) = text.strip_prefix("similarity index ") {
                let similarity = s.trim_end_matches('%').parse().unwrap_or(0);
                f.change = FileChange::Renamed { similarity };
            } else if let Some(p) = h.strip_prefix(b"--- ") {
                f.old_path = header_path(p, b"a/");
            } else if let Some(p) = h.strip_prefix(b"+++ ") {
                f.new_path = header_path(p, b"b/");
            } else if text.starts_with("Binary files ") || text.starts_with("GIT binary patch") {
                f.binary = true;
            }
        }
        if let FileChange::Renamed { similarity } = f.change
            && copied
        {
            f.change = FileChange::Copied { similarity };
        }
        // Hunks.
        while let Some(&h) = lines.peek() {
            if !h.starts_with(b"@@ ") {
                break;
            }
            lines.next();
            let text = String::from_utf8_lossy(h);
            let inner = text.strip_prefix("@@ ").ok_or_else(|| bad(h))?;
            let (ranges, section) = inner.split_once(" @@").ok_or_else(|| bad(h))?;
            let (old, new) = ranges.split_once(' ').ok_or_else(|| bad(h))?;
            let (old_start, old_len) = old
                .strip_prefix('-')
                .and_then(parse_range)
                .ok_or_else(|| bad(h))?;
            let (new_start, new_len) = new
                .strip_prefix('+')
                .and_then(parse_range)
                .ok_or_else(|| bad(h))?;
            let mut hunk = Hunk {
                old_start,
                old_len,
                new_start,
                new_len,
                section: section.trim_start().to_string(),
                lines: Vec::new(),
            };
            let (mut old_rem, mut new_rem) = (old_len, new_len);
            let (mut o, mut n) = (old_start, new_start);
            while old_rem > 0 || new_rem > 0 {
                let l = lines.next().ok_or("hunk ends early")?;
                let (kind, body) = match l.first() {
                    Some(b' ') => (DiffLineKind::Context, &l[1..]),
                    Some(b'+') => (DiffLineKind::Added, &l[1..]),
                    Some(b'-') => (DiffLineKind::Removed, &l[1..]),
                    Some(b'\\') => {
                        if let Some(last) = hunk.lines.last_mut() {
                            last.no_newline = true;
                        }
                        continue;
                    }
                    // An empty context line can lose its leading space.
                    None => (DiffLineKind::Context, l),
                    _ => return Err(bad(l)),
                };
                let (old_line, new_line) = match kind {
                    DiffLineKind::Context => (Some(o), Some(n)),
                    DiffLineKind::Added => (None, Some(n)),
                    DiffLineKind::Removed => (Some(o), None),
                };
                if old_line.is_some() {
                    o += 1;
                    old_rem = old_rem.checked_sub(1).ok_or("hunk too long")?;
                }
                if new_line.is_some() {
                    n += 1;
                    new_rem = new_rem.checked_sub(1).ok_or("hunk too long")?;
                }
                hunk.lines.push(DiffLine {
                    kind,
                    old_line,
                    new_line,
                    text: String::from_utf8_lossy(body).into_owned(),
                    no_newline: false,
                });
            }
            // A trailing "\ No newline at end of file" for the last line.
            if lines.peek().is_some_and(|l| l.starts_with(b"\\")) {
                lines.next();
                if let Some(last) = hunk.lines.last_mut() {
                    last.no_newline = true;
                }
            }
            f.hunks.push(hunk);
        }
        files.push(f);
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modify_add_delete_rename_binary() {
        let out = b"diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@ fn main()
 one
-two
+TWO
+three
 four
diff --git a/new file.txt b/new file.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new file.txt\t
@@ -0,0 +1 @@
+hello
\\ No newline at end of file
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 4444444..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/old.txt b/renamed.txt
similarity index 90%
rename from old.txt
rename to renamed.txt
index 5555555..6666666 100644
--- a/old.txt
+++ b/renamed.txt
@@ -1,2 +1,2 @@
 keep
--- not a header
+++ not a header either
diff --git a/img.png b/img.png
index 7777777..8888888 100644
Binary files a/img.png and b/img.png differ
diff --git \"a/tab\\there\" \"b/tab\\there\"
new file mode 100644
index 0000000..e69de29
";
        let files = parse_unified(out).unwrap();
        assert_eq!(files.len(), 6);

        let a = &files[0];
        assert_eq!(a.change, FileChange::Modified);
        assert_eq!(a.new_path, Some(PathBuf::from("src/a.rs")));
        let h = &a.hunks[0];
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (1, 3, 1, 4)
        );
        assert_eq!(h.section, "fn main()");
        let numbered: Vec<_> = h
            .lines
            .iter()
            .map(|l| (l.kind, l.old_line, l.new_line, l.text.as_str()))
            .collect();
        assert_eq!(
            numbered,
            vec![
                (DiffLineKind::Context, Some(1), Some(1), "one"),
                (DiffLineKind::Removed, Some(2), None, "two"),
                (DiffLineKind::Added, None, Some(2), "TWO"),
                (DiffLineKind::Added, None, Some(3), "three"),
                (DiffLineKind::Context, Some(3), Some(4), "four"),
            ]
        );
        assert_eq!(a.counts(), (2, 1));

        let n = &files[1];
        assert_eq!(n.change, FileChange::Added);
        assert_eq!(n.old_path, None);
        assert_eq!(n.new_path, Some(PathBuf::from("new file.txt")));
        assert!(n.hunks[0].lines[0].no_newline);

        let d = &files[2];
        assert_eq!(d.change, FileChange::Deleted);
        assert_eq!(d.new_path, None);
        assert_eq!(d.old_path, Some(PathBuf::from("gone.txt")));

        let r = &files[3];
        assert_eq!(r.change, FileChange::Renamed { similarity: 90 });
        assert_eq!(r.old_path, Some(PathBuf::from("old.txt")));
        assert_eq!(r.new_path, Some(PathBuf::from("renamed.txt")));
        // Content lines that look like headers stay content.
        assert_eq!(r.hunks[0].lines[1].text, "-- not a header");
        assert_eq!(r.hunks[0].lines[2].kind, DiffLineKind::Added);

        assert!(files[4].binary);
        assert_eq!(files[4].path(), Some(&PathBuf::from("img.png")));

        assert_eq!(files[5].new_path, Some(PathBuf::from("tab\there")));
        assert!(files[5].hunks.is_empty());
    }

    #[test]
    fn escaped_quotes_and_combined_diffs() {
        let out = b"diff --cc both.txt
index 1111111,2222222..0000000
--- a/both.txt
+++ b/both.txt
@@@ -1,1 -1,1 +1,5 @@@
++<<<<<<< ours
 +ours
++=======
+ theirs
++>>>>>>> theirs
diff --git \"a/a\\\"b.bin\" \"b/a\\\"b.bin\"
index 3333333..4444444 100644
Binary files \"a/a\\\"b.bin\" and \"b/a\\\"b.bin\" differ
";
        let files = parse_unified(out).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files[0].unmerged);
        assert_eq!(files[0].path(), Some(&PathBuf::from("both.txt")));
        assert!(files[1].binary);
        assert_eq!(files[1].old_path, Some(PathBuf::from("a\"b.bin")));
        assert_eq!(files[1].new_path, Some(PathBuf::from("a\"b.bin")));
    }

    #[test]
    fn unquotes_octal_and_escapes() {
        assert_eq!(unquote(b"\"caf\\303\\251\""), "café".as_bytes());
        assert_eq!(unquote(b"\"a\\\"b\\\\c\""), b"a\"b\\c");
        assert_eq!(unquote(b"plain"), b"plain");
    }
}
