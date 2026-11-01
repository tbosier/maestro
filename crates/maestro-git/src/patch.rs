//! Patches for part of a diff: whole hunks or chosen lines, built so that
//! `git apply` puts exactly that part into the index, or takes exactly that
//! part out of the index or the working tree.
//!
//! A [`FilePatch`] keeps the exact bytes of every diff line next to the
//! display model ([`DiffFile`]), so a file that is not UTF-8, has CRLF line
//! ends or lacks a final newline round-trips byte for byte. Lines are picked
//! by their index in [`DiffFile::hunks`]`[h].lines`.
//!
//! How lines that are not picked are written depends on which way the patch
//! will be applied ([`Side`]):
//!
//! | not picked | `Forward` (staging) | `Reverse` (unstaging, discarding) |
//! |---|---|---|
//! | added line | left out | kept as context |
//! | removed line | kept as context | left out |
//!
//! so the side the patch is applied to (the old side going forward, the new
//! side in reverse) is always the text that is really there.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::diff::parse_unified;
use crate::{DiffFile, DiffLineKind, LineHunk, LineHunkKind, diff_lines};

/// Which way a built patch will be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// As shown (`git apply`): staging an unstaged change.
    Forward,
    /// Undone (`git apply -R`): unstaging a staged change, or discarding a
    /// change from the working tree.
    Reverse,
}

/// Why a patch could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatchError {
    #[error("binary file: it can only be staged, unstaged or discarded as a whole")]
    Binary,
    #[error("file in conflict: resolve it, then stage the whole file")]
    Unmerged,
    #[error("the file has no hunk {0}")]
    NoSuchHunk(usize),
    #[error("no changed line is selected")]
    NothingSelected,
    /// Picking these lines would leave a line without a newline in the
    /// middle of the file.
    #[error(
        "the file's last line has no newline, and this selection would join it to the next line; \
         select the change to the last line too"
    )]
    JoinsLines,
}

/// What to take from each hunk: every changed line, or the lines at these
/// indices (into the hunk's `lines`). Context lines in a pick are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picks(BTreeMap<usize, Option<BTreeSet<usize>>>);

impl Picks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every changed line of hunk `h`.
    pub fn hunk(mut self, h: usize) -> Self {
        self.0.insert(h, None);
        self
    }

    /// The lines at `lines` in hunk `h`, added to any already picked there.
    pub fn lines(mut self, h: usize, lines: impl IntoIterator<Item = usize>) -> Self {
        let entry = self.0.entry(h).or_insert_with(|| Some(BTreeSet::new()));
        if let Some(set) = entry {
            set.extend(lines);
        }
        self
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn picked(&self, h: usize, line: usize) -> bool {
        match self.0.get(&h) {
            Some(None) => true,
            Some(Some(set)) => set.contains(&line),
            None => false,
        }
    }
}

/// One file's part of a diff, with the exact bytes of every line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    /// Paths, kind of change and numbered lines, for showing the diff.
    /// Line text there is decoded for display; patches use the bytes.
    pub file: DiffFile,
    /// This file's section of the diff output, exactly as git wrote it.
    pub raw: Vec<u8>,
    /// The bytes of each line (without its marker and newline; a CR before
    /// the newline stays), in the same order as `file.hunks[h].lines`.
    bytes: Vec<Vec<Vec<u8>>>,
}

/// Splits `git diff` output (with `a/` and `b/` prefixes) into files.
pub fn parse_patches(out: &[u8]) -> Result<Vec<FilePatch>, String> {
    // A file starts at a `diff --git` (or combined `diff --cc`) line. Lines
    // inside hunks start with ' ', '+', '-' or '\', so they never match.
    let mut starts = Vec::new();
    let mut at = 0;
    while at < out.len() {
        let line = &out[at..];
        if line.starts_with(b"diff --git ")
            || line.starts_with(b"diff --cc ")
            || line.starts_with(b"diff --combined ")
        {
            starts.push(at);
        }
        at = match out[at..].iter().position(|c| *c == b'\n') {
            Some(i) => at + i + 1,
            None => out.len(),
        };
    }
    if starts.is_empty() && out.iter().any(|c| !c.is_ascii_whitespace()) {
        return Err("diff output without a diff header".into());
    }
    let mut patches = Vec::with_capacity(starts.len());
    for (i, &s) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(out.len());
        let chunk = &out[s..end];
        let mut files = parse_unified(chunk)?;
        if files.len() != 1 {
            return Err(format!("expected one file, found {}", files.len()));
        }
        let file = files.remove(0);
        let bytes = hunk_bytes(chunk, &file)?;
        patches.push(FilePatch {
            file,
            raw: chunk.to_vec(),
            bytes,
        });
    }
    Ok(patches)
}

/// The raw bytes of every hunk line in `chunk`, walked with the same line
/// counts `parse_unified` used.
fn hunk_bytes(chunk: &[u8], file: &DiffFile) -> Result<Vec<Vec<Vec<u8>>>, String> {
    if file.unmerged {
        return Ok(Vec::new());
    }
    let mut lines = chunk.split(|c| *c == b'\n').peekable();
    let mut out = Vec::with_capacity(file.hunks.len());
    for h in &file.hunks {
        // Skip to this hunk's header.
        loop {
            match lines.next() {
                Some(l) if l.starts_with(b"@@ ") => break,
                Some(_) => {}
                None => return Err("hunk header missing".into()),
            }
        }
        let mut rows = Vec::with_capacity(h.lines.len());
        while rows.len() < h.lines.len() {
            let l = lines.next().ok_or("hunk ends early")?;
            if l.first() == Some(&b'\\') {
                continue;
            }
            rows.push(if l.is_empty() {
                Vec::new()
            } else {
                l[1..].to_vec()
            });
        }
        out.push(rows);
    }
    Ok(out)
}

impl FilePatch {
    /// The path a partial patch names: the file as it is on the side the
    /// patch applies to. For a rename that is the new path.
    pub fn target_path(&self) -> Option<&PathBuf> {
        self.file.new_path.as_ref().or(self.file.old_path.as_ref())
    }

    /// The exact bytes of line `line` of hunk `h`.
    pub fn line_bytes(&self, h: usize, line: usize) -> Option<&[u8]> {
        self.bytes.get(h)?.get(line).map(Vec::as_slice)
    }

    /// True when `picks` takes every changed line of every hunk.
    pub fn covers_all(&self, picks: &Picks) -> bool {
        self.file.hunks.iter().enumerate().all(|(h, hunk)| {
            hunk.lines
                .iter()
                .enumerate()
                .all(|(i, l)| l.kind == DiffLineKind::Context || picks.picked(h, i))
        })
    }

    /// A patch holding the picked lines, to be applied as `side` says.
    /// Paths are written as the target path on both sides, so the patch
    /// only changes the contents of one existing file (never its mode or
    /// name). Unpicked hunks are left out.
    pub fn build(&self, picks: &Picks, side: Side) -> Result<Vec<u8>, PatchError> {
        if self.file.binary {
            return Err(PatchError::Binary);
        }
        if self.file.unmerged {
            return Err(PatchError::Unmerged);
        }
        for &h in picks.0.keys() {
            if h >= self.file.hunks.len() {
                return Err(PatchError::NoSuchHunk(h));
            }
        }
        let path = self.target_path().ok_or(PatchError::NothingSelected)?;
        let mut body = Vec::new();
        // Lines the patch's earlier hunks add minus lines they remove.
        let mut delta: isize = 0;
        for &h in picks.0.keys() {
            let hunk = &self.file.hunks[h];
            let mut rows: Vec<(u8, &[u8], bool)> = Vec::new();
            let mut changed = false;
            for (i, l) in hunk.lines.iter().enumerate() {
                let bytes = self.bytes[h][i].as_slice();
                let picked = picks.picked(h, i);
                let marker = match (l.kind, picked, side) {
                    (DiffLineKind::Context, _, _) => Some(b' '),
                    (DiffLineKind::Added, true, _) => Some(b'+'),
                    (DiffLineKind::Removed, true, _) => Some(b'-'),
                    (DiffLineKind::Added, false, Side::Forward) => None,
                    (DiffLineKind::Removed, false, Side::Forward) => Some(b' '),
                    (DiffLineKind::Added, false, Side::Reverse) => Some(b' '),
                    (DiffLineKind::Removed, false, Side::Reverse) => None,
                };
                if let Some(m) = marker {
                    changed |= m != b' ';
                    rows.push((m, bytes, l.no_newline));
                }
            }
            if !changed {
                continue;
            }
            // Only the last line of each side may lack a newline.
            for side_markers in [*b" -", *b" +"] {
                let on_side: Vec<bool> = rows
                    .iter()
                    .filter(|r| side_markers.contains(&r.0))
                    .map(|r| r.2)
                    .collect();
                if on_side.iter().rev().skip(1).any(|nonl| *nonl) {
                    return Err(PatchError::JoinsLines);
                }
            }
            let old_len = rows.iter().filter(|r| r.0 != b'+').count();
            let new_len = rows.iter().filter(|r| r.0 != b'-').count();
            let (old_start, new_start) = match side {
                Side::Forward => {
                    // The old side is the original old side.
                    let pos = if hunk.old_len > 0 {
                        hunk.old_start - 1
                    } else {
                        hunk.old_start
                    };
                    let new_pos = (pos as isize + delta).max(0) as usize;
                    (
                        hunk.old_start,
                        if new_len > 0 { new_pos + 1 } else { new_pos },
                    )
                }
                Side::Reverse => {
                    // The new side is the original new side.
                    let pos = if hunk.new_len > 0 {
                        hunk.new_start - 1
                    } else {
                        hunk.new_start
                    };
                    let old_pos = (pos as isize - delta).max(0) as usize;
                    (
                        if old_len > 0 { old_pos + 1 } else { old_pos },
                        hunk.new_start,
                    )
                }
            };
            delta += new_len as isize - old_len as isize;
            body.extend_from_slice(
                format!("@@ -{old_start},{old_len} +{new_start},{new_len} @@\n").as_bytes(),
            );
            for (m, bytes, nonl) in rows {
                body.push(m);
                body.extend_from_slice(bytes);
                body.push(b'\n');
                if nonl {
                    body.extend_from_slice(b"\\ No newline at end of file\n");
                }
            }
        }
        if body.is_empty() {
            return Err(PatchError::NothingSelected);
        }
        let mut out = header(path);
        out.extend_from_slice(&body);
        Ok(out)
    }
}

/// `diff --git`, `---` and `+++` lines naming `path` on both sides.
fn header(path: &Path) -> Vec<u8> {
    let raw = path_bytes(path);
    let a = quote(b"a/", &raw);
    let b = quote(b"b/", &raw);
    // Git ends a ---/+++ name containing a space with a tab, so the name's
    // end is unambiguous; `git apply` reads it the same way.
    let tab: &[u8] = if raw.contains(&b' ') { b"\t" } else { b"" };
    let mut out = Vec::new();
    out.extend_from_slice(b"diff --git ");
    out.extend_from_slice(&a);
    out.push(b' ');
    out.extend_from_slice(&b);
    out.extend_from_slice(b"\n--- ");
    out.extend_from_slice(&a);
    out.extend_from_slice(tab);
    out.extend_from_slice(b"\n+++ ");
    out.extend_from_slice(&b);
    out.extend_from_slice(tab);
    out.push(b'\n');
    out
}

fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().replace('\\', "/").into_bytes()
    }
}

/// `prefix` and `path`, in git's C-style quotes when the path needs them.
fn quote(prefix: &[u8], path: &[u8]) -> Vec<u8> {
    let needs = path
        .iter()
        .any(|&c| c < 0x20 || c == 0x7f || c >= 0x80 || c == b'"' || c == b'\\');
    let mut out = Vec::new();
    if !needs {
        out.extend_from_slice(prefix);
        out.extend_from_slice(path);
        return out;
    }
    out.push(b'"');
    out.extend_from_slice(prefix);
    for &c in path {
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            c if !(0x20..0x7f).contains(&c) => {
                out.extend_from_slice(format!("\\{c:03o}").as_bytes())
            }
            c => out.push(c),
        }
    }
    out.push(b'"');
    out
}

/// A patch computed in process between two texts, with the changes the
/// gutter signs show ([`diff_lines`]) mapped onto its lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPatch {
    pub patch: FilePatch,
    /// The changes, as [`diff_lines`] gives them for the two texts.
    pub changes: Vec<LineHunk>,
    /// For each line of each hunk, the change it belongs to (`None` for
    /// context).
    pub change_of: Vec<Vec<Option<usize>>>,
}

/// Lines of `text` with their terminators, as byte slices.
fn byte_lines(text: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        match rest.iter().position(|c| *c == b'\n') {
            Some(i) => {
                out.push(&rest[..=i]);
                rest = &rest[i + 1..];
            }
            None => {
                out.push(rest);
                rest = &[];
            }
        }
    }
    out
}

impl TextPatch {
    /// The diff from `old` to `new` (the exact bytes of two versions of
    /// `path`) with three lines of context, hunks joined as git joins them.
    /// The changes are the ones [`diff_lines`] finds, so they match the
    /// gutter signs for the same texts.
    pub fn new(path: &Path, old: &[u8], new: &[u8]) -> Result<Self, String> {
        const CONTEXT: usize = 3;
        let a = byte_lines(old);
        let b = byte_lines(new);
        let changes = diff_lines(&String::from_utf8_lossy(old), &String::from_utf8_lossy(new));
        // Group changes whose context would touch or overlap.
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (i, c) in changes.iter().enumerate() {
            match groups.last_mut() {
                Some(g)
                    if c.old.start
                        <= changes[*g.last().expect("nonempty")].old.end + 2 * CONTEXT =>
                {
                    g.push(i)
                }
                _ => groups.push(vec![i]),
            }
        }
        let mut raw = header(path);
        let mut change_of = Vec::new();
        let push_line = |raw: &mut Vec<u8>, marker: u8, line: &[u8]| {
            raw.push(marker);
            match line.strip_suffix(b"\n") {
                Some(body) => {
                    raw.extend_from_slice(body);
                    raw.push(b'\n');
                }
                None => {
                    raw.extend_from_slice(line);
                    raw.extend_from_slice(b"\n\\ No newline at end of file\n");
                }
            }
        };
        for g in &groups {
            let first = &changes[g[0]];
            let last = &changes[*g.last().expect("nonempty")];
            let old_from = first.old.start.saturating_sub(CONTEXT);
            let old_to = (last.old.end + CONTEXT).min(a.len());
            // Context before the first change is the same on both sides.
            let new_from = first.new.start - (first.old.start - old_from);
            let new_to = (last.new.end + (old_to - last.old.end)).min(b.len());
            let old_len = old_to - old_from;
            let new_len = new_to - new_from;
            let old_start = if old_len > 0 { old_from + 1 } else { old_from };
            let new_start = if new_len > 0 { new_from + 1 } else { new_from };
            raw.extend_from_slice(
                format!("@@ -{old_start},{old_len} +{new_start},{new_len} @@\n").as_bytes(),
            );
            let mut owners = Vec::new();
            let mut o = old_from;
            for &ci in g {
                let c = &changes[ci];
                while o < c.old.start {
                    push_line(&mut raw, b' ', a[o]);
                    owners.push(None);
                    o += 1;
                }
                for line in &a[c.old.clone()] {
                    push_line(&mut raw, b'-', line);
                    owners.push(Some(ci));
                }
                for line in &b[c.new.clone()] {
                    push_line(&mut raw, b'+', line);
                    owners.push(Some(ci));
                }
                o = c.old.end;
            }
            while o < old_to {
                push_line(&mut raw, b' ', a[o]);
                owners.push(None);
                o += 1;
            }
            change_of.push(owners);
        }
        if groups.is_empty() {
            // Nothing changed: an empty patch with just the header.
            raw.clear();
        }
        let mut patches = parse_patches(&raw)?;
        let patch = match patches.pop() {
            Some(p) => p,
            None => FilePatch {
                file: DiffFile {
                    old_path: Some(path.to_path_buf()),
                    new_path: Some(path.to_path_buf()),
                    change: crate::FileChange::Modified,
                    old_mode: None,
                    new_mode: None,
                    binary: false,
                    unmerged: false,
                    hunks: Vec::new(),
                },
                raw: Vec::new(),
                bytes: Vec::new(),
            },
        };
        Ok(Self {
            patch,
            changes,
            change_of,
        })
    }

    /// The change (index into `changes`) shown at zero-based new-text line
    /// `line`: the change containing it, or a removal whose gutter sign sits
    /// on it (removed lines are marked on the line above them).
    pub fn change_at(&self, line: usize) -> Option<usize> {
        self.changes
            .iter()
            .position(|c| c.new.contains(&line))
            .or_else(|| {
                self.changes.iter().position(|c| {
                    c.kind == LineHunkKind::Removed && c.new.start.saturating_sub(1) == line
                })
            })
    }

    /// Picks every line of change `change`.
    pub fn pick_change(&self, change: usize) -> Picks {
        let mut picks = Picks::new();
        for (h, owners) in self.change_of.iter().enumerate() {
            let lines: Vec<usize> = owners
                .iter()
                .enumerate()
                .filter(|(_, o)| **o == Some(change))
                .map(|(i, _)| i)
                .collect();
            if !lines.is_empty() {
                picks = picks.lines(h, lines);
            }
        }
        picks
    }

    /// Picks the changes to new-text lines `from..=to` (zero-based): each
    /// added line in the range, the removed line paired with it (the k-th
    /// removed line goes with the k-th added line of the same change), the
    /// extra removed lines of a change whose last added line is in the
    /// range, and removals whose sign sits on a line in the range.
    pub fn pick_new_lines(&self, from: usize, to: usize) -> Picks {
        let in_range = |l: usize| l >= from && l <= to;
        let mut picks = Picks::new();
        for (h, owners) in self.change_of.iter().enumerate() {
            let lines = &self.patch.file.hunks[h].lines;
            let mut chosen = Vec::new();
            for (i, owner) in owners.iter().enumerate() {
                let Some(ci) = *owner else { continue };
                let c = &self.changes[ci];
                let take = match lines[i].kind {
                    DiffLineKind::Added => {
                        let n = lines[i].new_line.unwrap_or(0).saturating_sub(1);
                        in_range(n)
                    }
                    DiffLineKind::Removed => {
                        let k = lines[i].old_line.unwrap_or(0).saturating_sub(1) - c.old.start;
                        if c.new.is_empty() {
                            in_range(c.new.start.saturating_sub(1))
                        } else if k < c.new.len() {
                            in_range(c.new.start + k)
                        } else {
                            in_range(c.new.end - 1)
                        }
                    }
                    DiffLineKind::Context => false,
                };
                if take {
                    chosen.push(i);
                }
            }
            if !chosen.is_empty() {
                picks = picks.lines(h, chosen);
            }
        }
        picks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &[u8] = b"diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,4 +1,4 @@
 one
-two
-three
+TWO
+THREE
 four
@@ -10,3 +10,4 @@ section
 ten
+ten and a half
 eleven
 twelve
";

    fn one(raw: &[u8]) -> FilePatch {
        let mut v = parse_patches(raw).unwrap();
        assert_eq!(v.len(), 1);
        v.remove(0)
    }

    fn text(b: Vec<u8>) -> String {
        String::from_utf8(b).unwrap()
    }

    #[test]
    fn whole_hunk_forward_and_reverse() {
        let p = one(DIFF);
        let fwd = text(p.build(&Picks::new().hunk(1), Side::Forward).unwrap());
        assert_eq!(
            fwd,
            "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n\
             @@ -10,3 +10,4 @@\n ten\n+ten and a half\n eleven\n twelve\n"
        );
        // Reverse keeps the new side's numbers; the old side moves by what
        // earlier hunks in the patch change (none here).
        let rev = text(p.build(&Picks::new().hunk(1), Side::Reverse).unwrap());
        assert!(rev.contains("@@ -10,3 +10,4 @@\n"), "{rev}");
    }

    #[test]
    fn picked_lines_forward_turn_unpicked_removals_into_context() {
        let p = one(DIFF);
        // Lines of hunk 0: 0 " one", 1 "-two", 2 "-three", 3 "+TWO", 4 "+THREE", 5 " four"
        let fwd = text(
            p.build(&Picks::new().lines(0, [1, 3]), Side::Forward)
                .unwrap(),
        );
        assert!(
            fwd.ends_with("@@ -1,4 +1,4 @@\n one\n-two\n three\n+TWO\n four\n"),
            "{fwd}"
        );
        // Both hunks, second whole: the second's new start follows the
        // first's change in length (0 here, then +1 for a lone addition).
        let fwd = text(
            p.build(&Picks::new().lines(0, [3]).hunk(1), Side::Forward)
                .unwrap(),
        );
        assert!(
            fwd.contains("@@ -1,4 +1,5 @@\n one\n two\n three\n+TWO\n four\n@@ -10,3 +11,4 @@"),
            "{fwd}"
        );
    }

    #[test]
    fn picked_lines_reverse_turn_unpicked_additions_into_context() {
        let p = one(DIFF);
        let rev = text(
            p.build(&Picks::new().lines(0, [2, 4]), Side::Reverse)
                .unwrap(),
        );
        // New side unchanged (4 lines: one TWO THREE four); old side drops
        // "-two" (unpicked removal) and keeps "-three".
        assert!(
            rev.ends_with("@@ -1,4 +1,4 @@\n one\n-three\n TWO\n+THREE\n four\n"),
            "{rev}"
        );
        // Reverse with an earlier hunk that adds a net line: the later
        // hunk's old start moves back by it.
        let rev = text(
            p.build(&Picks::new().lines(0, [3]).hunk(1), Side::Reverse)
                .unwrap(),
        );
        assert!(
            rev.contains("@@ -1,3 +1,4 @@\n one\n+TWO\n THREE\n four\n"),
            "{rev}"
        );
        assert!(rev.contains("@@ -9,3 +10,4 @@\n"), "{rev}");
    }

    #[test]
    fn nothing_picked_and_bad_hunks_are_errors() {
        let p = one(DIFF);
        assert_eq!(
            p.build(&Picks::new().lines(0, [0, 5]), Side::Forward),
            Err(PatchError::NothingSelected)
        );
        assert_eq!(
            p.build(&Picks::new().hunk(7), Side::Forward),
            Err(PatchError::NoSuchHunk(7))
        );
    }

    #[test]
    fn missing_newline_markers_stay_with_their_line() {
        let raw = b"diff --git a/f b/f
--- a/f
+++ b/f
@@ -1,2 +1,3 @@
 a
-b
\\ No newline at end of file
+b
+c
\\ No newline at end of file
";
        let p = one(raw);
        assert!(p.file.hunks[0].lines[1].no_newline);
        // All of it round-trips.
        let all = p.build(&Picks::new().hunk(0), Side::Forward).unwrap();
        assert!(text(all).ends_with(
            " a\n-b\n\\ No newline at end of file\n+b\n+c\n\\ No newline at end of file\n"
        ));
        // Only "+c": "b" without a newline would sit in the middle.
        assert_eq!(
            p.build(&Picks::new().lines(0, [3]), Side::Forward),
            Err(PatchError::JoinsLines)
        );
        // The newline change alone is fine: the new side ends at "b".
        let nl = text(
            p.build(&Picks::new().lines(0, [1, 2]), Side::Forward)
                .unwrap(),
        );
        assert!(
            nl.ends_with("@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n"),
            "{nl}"
        );
    }

    #[test]
    fn crlf_and_non_utf8_bytes_round_trip() {
        let raw = b"diff --git a/w.txt b/w.txt
--- a/w.txt
+++ b/w.txt
@@ -1,2 +1,2 @@
 caf\xe9\r
-old\r
+new\r
";
        let p = one(raw);
        assert_eq!(p.line_bytes(0, 0), Some(&b"caf\xe9\r"[..]));
        let out = p.build(&Picks::new().lines(0, [2]), Side::Forward).unwrap();
        assert!(out.ends_with(b"@@ -1,2 +1,3 @@\n caf\xe9\r\n old\r\n+new\r\n"));
    }

    #[test]
    fn quoted_paths_and_binary_refusal() {
        assert_eq!(quote(b"a/", b"plain.txt"), b"a/plain.txt");
        assert_eq!(quote(b"a/", b"t\tab\"q"), b"\"a/t\\tab\\\"q\"");
        assert_eq!(quote(b"b/", "é".as_bytes()), b"\"b/\\303\\251\"");
        let h = text(header(Path::new("has space.txt")));
        assert!(h.contains("--- a/has space.txt\t\n"), "{h}");
        let bin = one(b"diff --git a/i.png b/i.png\nindex 1..2 100644\nBinary files a/i.png and b/i.png differ\n");
        assert_eq!(
            bin.build(&Picks::new().hunk(0), Side::Forward),
            Err(PatchError::Binary)
        );
    }

    #[test]
    fn text_patches_follow_the_sign_changes() {
        let old = b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n";
        let new = b"1\nTWO\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n";
        let tp = TextPatch::new(Path::new("n.txt"), old, new).unwrap();
        assert_eq!(tp.changes.len(), 2);
        assert_eq!(tp.patch.file.hunks.len(), 2);
        assert_eq!(tp.change_at(1), Some(0));
        assert_eq!(tp.change_at(20), Some(1));
        assert_eq!(tp.change_at(5), None);
        let only_second = text(tp.patch.build(&tp.pick_change(1), Side::Forward).unwrap());
        assert!(
            only_second.ends_with("@@ -18,3 +18,4 @@\n 18\n 19\n 20\n+21\n"),
            "{only_second}"
        );

        // Changes close together share a hunk; each can still be picked.
        let new2 = b"1\nTWO\n3\nFOUR\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n";
        let tp = TextPatch::new(Path::new("n.txt"), old, new2).unwrap();
        assert_eq!(tp.patch.file.hunks.len(), 1);
        let first = text(tp.patch.build(&tp.pick_change(0), Side::Forward).unwrap());
        assert!(
            first.ends_with("@@ -1,7 +1,7 @@\n 1\n-2\n+TWO\n 3\n 4\n 5\n 6\n 7\n"),
            "{first}"
        );
    }

    #[test]
    fn text_patch_line_picks_pair_removed_and_added_lines() {
        let old = b"a\nb\nc\nd\n";
        let new = b"a\nB\nC\nd\n";
        let tp = TextPatch::new(Path::new("p"), old, new).unwrap();
        // Line 2 ("C") pairs with removed "c".
        let p = text(
            tp.patch
                .build(&tp.pick_new_lines(2, 2), Side::Forward)
                .unwrap(),
        );
        assert!(p.ends_with("@@ -1,4 +1,4 @@\n a\n b\n-c\n+C\n d\n"), "{p}");
        // A file without a final newline keeps its marker.
        let tp = TextPatch::new(Path::new("p"), b"x\ny", b"x\nY").unwrap();
        let p = text(tp.patch.build(&tp.pick_change(0), Side::Forward).unwrap());
        assert!(
            p.ends_with(" x\n-y\n\\ No newline at end of file\n+Y\n\\ No newline at end of file\n"),
            "{p}"
        );
    }
}
