//! Applying edits from language servers: workspace edits from rename, code
//! actions and `workspace/applyEdit`, and text edits from formatting.
//!
//! An edit is all or nothing. Every part is checked before any buffer
//! changes:
//!
//! * a versioned document edit must match its open buffer's version (the
//!   version the editor last sent the server);
//! * a file that is not open must be a regular file no larger than
//!   [`MAX_OPEN_BYTES`]; it is read into a new buffer (not shown in any
//!   view) and edited there, never written. The user saves;
//! * no buffer may be read-only;
//! * every range must be inside the text (a character past the end of a
//!   line clamps to the line end, as the protocol says; a line past the end
//!   of the document is refused) and no two ranges in one document may
//!   overlap. Inserts at the same position apply in the order given.
//!
//! Each changed buffer gets one undo step, through
//! [`crate::proposals::apply_versioned`], which also moves cursors in every
//! view of it and tells language servers.

use std::path::PathBuf;

use maestro_core::pos::lsp_to_char;
use maestro_core::{Change, ChangeSet, Document, DocumentVersion, Rope};
use maestro_lsp::Value;
use maestro_lsp::convert::{parse_position, uri_to_path};
use maestro_lsp::edit::{DocumentEdit, TextEdit};

use crate::editor::{DocumentId, Editor, normalize_path};

/// Largest file an edit may open as a new buffer.
pub const MAX_OPEN_BYTES: u64 = 16 * 1024 * 1024;

/// What an applied edit did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Buffers whose text changed, in the order the edit listed them.
    pub changed: Vec<DocumentId>,
    /// How many of those were opened for the edit.
    pub opened: usize,
    /// Text edits applied, over all buffers, not counting edits that
    /// changed nothing.
    pub edits: usize,
    /// Warnings about buffers opened for the edit (unsaved changes from
    /// another session, say).
    pub warnings: Vec<String>,
}

impl Applied {
    /// "2 files", "1 file".
    pub fn files(&self) -> String {
        plural(self.changed.len(), "file")
    }

    /// The part of a status message every edit shares: how many files
    /// changed and that nothing was saved.
    pub fn summary(&self) -> String {
        let mut s = format!("{} changed, not saved", self.files());
        if self.opened > 0 {
            s.push_str(&format!(
                " ({} opened in the background; :wa saves all)",
                plural(self.opened, "file")
            ));
        }
        for w in &self.warnings {
            s.push_str("; ");
            s.push_str(w);
        }
        s
    }
}

pub(crate) fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Applies a protocol `WorkspaceEdit`. On error nothing changed.
pub fn apply_workspace_edit(ed: &mut Editor, edit: &Value) -> Result<Applied, String> {
    let docs = maestro_lsp::edit::parse_workspace_edit(edit)?;
    apply_document_edits(ed, docs, false)
}

enum Target {
    Open(DocumentId),
    New(Box<Document>),
}

struct Planned {
    target: Target,
    version: DocumentVersion,
    changes: ChangeSet,
    edits: usize,
}

/// Applies parsed document edits. With `refine`, an edit that replaces
/// whole runs of lines (a formatter answering with the full text, say) is
/// split into the lines that really differ, so cursors and marks on
/// unchanged lines stay where they were. On error nothing changed.
pub(crate) fn apply_document_edits(
    ed: &mut Editor,
    docs: Vec<DocumentEdit>,
    refine: bool,
) -> Result<Applied, String> {
    let plan = plan(ed, docs, refine)?;
    execute(ed, plan)
}

fn display(ed: &Editor, path: &std::path::Path) -> String {
    path.strip_prefix(&ed.cwd)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Checks everything and computes each document's change set, without
/// changing anything.
fn plan(ed: &Editor, docs: Vec<DocumentEdit>, refine: bool) -> Result<Vec<Planned>, String> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out = Vec::new();
    for d in docs {
        let path = uri_to_path(&d.uri).ok_or_else(|| format!("cannot edit {}", d.uri))?;
        let path = normalize_path(&path);
        let shown = display(ed, &path);
        if seen.contains(&path) {
            return Err(format!("the edit names {shown} twice"));
        }
        seen.push(path.clone());
        let (target, text, version) = match ed.find_buffer_by_path(&path) {
            Some(id) => {
                let doc = &ed.buffers[&id].doc;
                if let Some(v) = d.version
                    && u64::try_from(v).ok() != Some(doc.version().0)
                {
                    return Err(format!(
                        "{shown} changed since the server made the edit \
                         (it was for version {v}, the buffer is at version {})",
                        doc.version().0
                    ));
                }
                if d.edits.is_empty() {
                    continue;
                }
                if let Some(r) = doc.read_only() {
                    return Err(format!("{shown} is read-only ({})", r.describe()));
                }
                (Target::Open(id), doc.text().clone(), doc.version())
            }
            None => {
                if d.edits.is_empty() {
                    continue;
                }
                if let Some(v) = d.version {
                    return Err(format!(
                        "the server expected {shown} to be open at version {v}, but it is not open"
                    ));
                }
                let meta =
                    std::fs::metadata(&path).map_err(|e| format!("cannot open {shown}: {e}"))?;
                if !meta.is_file() {
                    return Err(format!("{shown} is not a regular file"));
                }
                if meta.len() > MAX_OPEN_BYTES {
                    return Err(format!("{shown} is too large to open for an edit"));
                }
                let doc = Document::open(&path).map_err(|e| format!("{shown}: {e}"))?;
                if let Some(r) = doc.read_only() {
                    return Err(format!("{shown} is read-only ({})", r.describe()));
                }
                let text = doc.text().clone();
                let version = doc.version();
                (Target::New(Box::new(doc)), text, version)
            }
        };
        let (changes, edits) =
            changes_for(&text, &d.edits, refine).map_err(|e| format!("{shown}: {e}"))?;
        if changes.is_empty() {
            continue;
        }
        out.push(Planned {
            target,
            version,
            changes,
            edits,
        });
    }
    Ok(out)
}

/// Converts one document's text edits into a change set over `text`, and
/// counts the edits that change something.
pub(crate) fn changes_for(
    text: &Rope,
    edits: &[TextEdit],
    refine: bool,
) -> Result<(ChangeSet, usize), String> {
    if edits.is_empty() {
        return Ok((ChangeSet::identity(text.len_chars()), 0));
    }
    // The editor's line numbers come from Ropey, which also breaks lines at
    // form feed, vertical tab, NEL and the Unicode line and paragraph
    // separators; the protocol counts only LF, CRLF and CR. With any of
    // those in the text, a server's line numbers would land on the wrong
    // lines, so the edit is refused.
    if let Some(c) = text
        .chars()
        .find(|c| matches!(c, '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}'))
    {
        return Err(format!(
            "the text contains U+{:04X}, a line break the editor and the protocol count \
             differently",
            c as u32
        ));
    }
    let mut list: Vec<(usize, usize, String)> = Vec::with_capacity(edits.len());
    for e in edits {
        let (start, end) = match (
            e.range.get("start").and_then(parse_position),
            e.range.get("end").and_then(parse_position),
        ) {
            (Some(s), Some(e)) => (s, e),
            _ => return Err("an edit has a malformed range".into()),
        };
        // Line numbers in messages are one-based; u64 so the largest u32
        // line cannot overflow.
        if (start.line, start.character) > (end.line, end.character) {
            return Err(format!(
                "an edit's range ends before it starts (line {})",
                u64::from(start.line) + 1
            ));
        }
        let (Some(from), Some(to)) = (lsp_to_char(text, start), lsp_to_char(text, end)) else {
            return Err(format!(
                "an edit reaches line {}, past the end of the text ({} lines)",
                u64::from(end.line) + 1,
                text.len_lines()
            ));
        };
        list.push((from, to, e.new_text.clone()));
    }
    // Stable: inserts at one position keep the server's order.
    list.sort_by_key(|&(from, to, _)| (from, to));
    if let Some(w) = list.windows(2).find(|w| w[0].1 > w[1].0) {
        return Err(format!(
            "two edits overlap at line {}",
            text.char_to_line(w[1].0) + 1
        ));
    }
    list.retain(|(from, to, new)| text.slice(*from..*to) != new.as_str());
    let count = list.len();
    if refine {
        list = list
            .into_iter()
            .flat_map(|(from, to, new)| {
                let old = text.slice(from..to).to_string();
                match refine_lines(&old, &new) {
                    Some(parts) => parts
                        .into_iter()
                        .map(|(a, b, t)| (from + a, from + b, t))
                        .collect(),
                    None => vec![(from, to, new)],
                }
            })
            .collect();
    }
    let changes = ChangeSet::from_changes(
        text.len_chars(),
        list.into_iter()
            .map(|(from, to, new)| Change::replace(from, to, new)),
    )
    .map_err(|e| e.to_string())?;
    Ok((changes, count))
}

/// Splits a replacement of `old` by `new` into replacements of only the
/// lines that differ, as char ranges in `old`. `None` when it spans no
/// line break (nothing to gain) or the split does not rebuild `new`
/// exactly.
fn refine_lines(old: &str, new: &str) -> Option<Vec<(usize, usize, String)>> {
    if !old.contains('\n') && !new.contains('\n') {
        return None;
    }
    let ol = crate::proposals::review::split_lines(old);
    let nl = crate::proposals::review::split_lines(new);
    let hunks =
        crate::proposals::review::exact_changes(&ol, &nl, maestro_git::diff_lines(old, new));
    // Char offset of each old line's start, plus the end.
    let mut starts = vec![0];
    for (line, newline) in &ol {
        let last = *starts.last().expect("non-empty");
        starts.push(last + line.chars().count() + usize::from(*newline));
    }
    let parts: Vec<(usize, usize, String)> = hunks
        .iter()
        .map(|h| {
            let text: String = nl[h.new.clone()]
                .iter()
                .map(|(l, n)| if *n { format!("{l}\n") } else { l.to_string() })
                .collect();
            (starts[h.old.start], starts[h.old.end], text)
        })
        .collect();
    let mut rebuilt = Rope::from_str(old);
    for (a, b, t) in parts.iter().rev() {
        if *a > *b || *b > rebuilt.len_chars() {
            return None;
        }
        rebuilt.remove(*a..*b);
        rebuilt.insert(*a, t);
    }
    (rebuilt == new).then_some(parts)
}

/// Applies a checked plan. Nothing in it can fail after the checks, short
/// of a bug; if one does, the error says which files were already changed.
fn execute(ed: &mut Editor, plan: Vec<Planned>) -> Result<Applied, String> {
    let mut applied = Applied::default();
    for p in plan {
        let id = match p.target {
            Target::Open(id) => id,
            Target::New(doc) => {
                let id = ed.add_document(*doc);
                applied.opened += 1;
                // Report unsaved work from another session without losing
                // the caller's status message.
                let before = ed.status.take();
                crate::recovery::notify_if_recoverable(ed, id);
                if let Some((sev, w)) = ed.status.take() {
                    // Logged once, as part of the edit's own message.
                    ed.messages.unrecord(sev, &w);
                    applied.warnings.push(w);
                }
                ed.status = before;
                id
            }
        };
        if let Err(e) = crate::proposals::apply_versioned(ed, id, p.version, &p.changes) {
            let name = ed.buffers[&id].doc.display_name();
            return Err(format!(
                "{name}: {e}; {} already changed",
                plural(applied.changed.len(), "other file")
            ));
        }
        applied.changed.push(id);
        applied.edits += p.edits;
    }
    super::reconcile(ed);
    ed.ensure_cursor_visible();
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_lsp::json;

    fn edit(l1: u32, c1: u32, l2: u32, c2: u32, t: &str) -> TextEdit {
        TextEdit {
            range: json!({"start": {"line": l1, "character": c1}, "end": {"line": l2, "character": c2}}),
            new_text: t.to_string(),
        }
    }

    fn apply(text: &str, edits: &[TextEdit], refine: bool) -> Result<String, String> {
        let rope = Rope::from_str(text);
        let (cs, _) = changes_for(&rope, edits, refine)?;
        let mut out = rope.clone();
        cs.apply(&mut out).unwrap();
        Ok(out.to_string())
    }

    #[test]
    fn edits_apply_against_the_original_text() {
        let text = "fn a() {}\nfn b() { a(); }\n";
        // Unsorted, on two lines, with two inserts at one spot kept in order.
        let edits = [
            edit(1, 9, 1, 10, "z"),
            edit(0, 3, 0, 4, "z"),
            edit(0, 0, 0, 0, "// 1\n"),
            edit(0, 0, 0, 0, "// 2\n"),
        ];
        assert_eq!(
            apply(text, &edits, false).unwrap(),
            "// 1\n// 2\nfn z() {}\nfn b() { z(); }\n"
        );
        // A character past the line end clamps to it.
        assert_eq!(
            apply("ab\ncd\n", &[edit(0, 99, 0, 99, "!")], false).unwrap(),
            "ab!\ncd\n"
        );
        // UTF-16: the emoji is two units.
        assert_eq!(
            apply("😀x\n", &[edit(0, 2, 0, 3, "y")], false).unwrap(),
            "😀y\n"
        );
    }

    #[test]
    fn bad_ranges_are_refused() {
        let text = "abc\ndef\n";
        let e = apply(text, &[edit(0, 0, 0, 2, "x"), edit(0, 1, 0, 3, "y")], false);
        assert!(e.unwrap_err().contains("overlap"));
        // The same non-empty range twice overlaps too.
        let e = apply(text, &[edit(1, 0, 1, 1, "x"), edit(1, 0, 1, 1, "y")], false);
        assert!(e.unwrap_err().contains("overlap at line 2"));
        let e = apply(text, &[edit(9, 0, 9, 0, "x")], false);
        assert!(e.unwrap_err().contains("past the end"));
        let e = apply(text, &[edit(1, 2, 0, 0, "x")], false);
        assert!(e.unwrap_err().contains("ends before it starts"));
        // The largest line numbers are refused, not overflowed.
        let e = apply(text, &[edit(u32::MAX, 0, u32::MAX, 0, "x")], false);
        assert!(e.unwrap_err().contains("line 4294967296"));
        let e = apply(text, &[edit(u32::MAX, 0, 0, 0, "x")], false);
        assert!(e.unwrap_err().contains("ends before it starts"));
        // A form feed is a line break to the editor but not to the
        // protocol: line 1 would be `foo` here instead of `bar();`.
        let e = apply("//\u{0C}foo\nbar();\n", &[edit(1, 0, 1, 3, "baz")], false);
        assert!(e.unwrap_err().contains("U+000C"));
        // Touching edits and an insert at a replacement's start are fine.
        assert_eq!(
            apply(
                text,
                &[
                    edit(0, 0, 0, 1, "A"),
                    edit(0, 1, 0, 2, "B"),
                    edit(0, 0, 0, 0, "<")
                ],
                false
            )
            .unwrap(),
            "<ABc\ndef\n"
        );
    }

    #[test]
    fn refining_keeps_unchanged_lines_out_of_the_edit() {
        let old = "a  \nb\nc\t\nd\n";
        let new = "a\nb\nc\nd\n";
        let whole = [edit(0, 0, 4, 0, new)];
        assert_eq!(apply(old, &whole, true).unwrap(), new);
        let rope = Rope::from_str(old);
        let (cs, n) = changes_for(&rope, &whole, true).unwrap();
        assert_eq!(n, 1, "one server edit");
        let parts = cs.changes();
        assert_eq!(parts.len(), 2, "{parts:?}");
        // Line `b` (chars 4..6) is untouched: a cursor there stays.
        assert_eq!(cs.map_pos(4, maestro_core::Assoc::After), 2);
        // CRLF and a missing final newline rebuild exactly.
        let old = "x \r\ny\r\nz";
        let new = "x\r\ny\r\nz\n";
        assert_eq!(apply(old, &[edit(0, 0, 2, 1, new)], true).unwrap(), new);
        // Edits that change nothing are dropped.
        let (cs, n) = changes_for(&Rope::from_str("ab"), &[edit(0, 0, 0, 2, "ab")], true).unwrap();
        assert!(cs.is_empty());
        assert_eq!(n, 0);
    }
}
