//! A document: text plus everything needed to edit and save it safely.
//!
//! Each applied change bumps [`DocumentVersion`]. Asynchronous producers of
//! edits (language servers, agents, the automation API) record the version
//! they computed against and submit through [`Document::apply_at_version`],
//! which refuses stale edits instead of applying them at shifted offsets.

use std::io;
use std::path::{Path, PathBuf};

use ropey::Rope;
use thiserror::Error;

use crate::change::{ChangeError, ChangeSet, Transaction};
use crate::history::History;
use crate::io::{DiskState, Encoding};
use crate::line_ending::LineEnding;
use crate::selection::Selection;

/// Monotonic per-document version. Increases on every applied change,
/// including undo and redo, and never repeats within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocumentVersion(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnlyReason {
    Binary,
    InvalidEncoding,
    Permission,
}

impl ReadOnlyReason {
    pub fn describe(self) -> &'static str {
        match self {
            ReadOnlyReason::Binary => "binary file",
            ReadOnlyReason::InvalidEncoding => "not valid in a supported encoding",
            ReadOnlyReason::Permission => "file is not writable",
        }
    }
}

#[derive(Debug, Error)]
pub enum EditError {
    #[error("document is read-only: {}", .0.describe())]
    ReadOnly(ReadOnlyReason),
    #[error("edit was computed against version {expected:?} but the document is at {actual:?}")]
    Stale {
        expected: DocumentVersion,
        actual: DocumentVersion,
    },
    #[error(transparent)]
    Change(#[from] ChangeError),
}

#[derive(Debug, Error)]
pub enum SaveError {
    #[error("no file name; use :w <path>")]
    NoPath,
    #[error("document is read-only: {}", .0.describe())]
    ReadOnly(ReadOnlyReason),
    #[error("{} changed on disk since it was opened; use :w! to overwrite or :reload", .0.display())]
    ExternallyModified(PathBuf),
    #[error("{0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone)]
struct Pending {
    changes: Vec<ChangeSet>,
    inverses: Vec<ChangeSet>,
    selection_before: Selection,
}

/// The result of an undo or redo: the change sets that were applied (so
/// other views can map their selections) and the selection to restore.
#[derive(Debug, Clone)]
pub struct HistoryStep {
    pub applied: Vec<ChangeSet>,
    pub selection: Selection,
}

#[derive(Debug, Clone)]
pub struct Document {
    text: Rope,
    path: Option<PathBuf>,
    encoding: Encoding,
    line_ending: LineEnding,
    version: u64,
    history: History,
    pending: Option<Pending>,
    /// History revision that matches the file on disk. `None` means no
    /// revision does (for example after a forced reload failed midway).
    saved_revision: Option<usize>,
    disk: Option<DiskState>,
    read_only: Option<ReadOnlyReason>,
}

impl Default for Document {
    fn default() -> Self {
        Self::from_text("")
    }
}

impl Document {
    /// An unnamed scratch document.
    pub fn from_text(text: &str) -> Self {
        let text = Rope::from_str(text);
        let line_ending = LineEnding::detect(&text);
        Self {
            text,
            path: None,
            encoding: Encoding::Utf8,
            line_ending,
            version: 0,
            history: History::default(),
            pending: None,
            saved_revision: Some(0),
            disk: None,
            read_only: None,
        }
    }

    /// Opens a file. A path that does not exist yet becomes an empty document
    /// that will create the file on first save.
    pub fn open(path: &Path) -> io::Result<Self> {
        match crate::io::load(path) {
            Ok(loaded) => {
                let line_ending = LineEnding::detect(&loaded.text);
                Ok(Self {
                    text: loaded.text,
                    path: Some(path.to_path_buf()),
                    encoding: loaded.encoding,
                    line_ending,
                    version: 0,
                    history: History::default(),
                    pending: None,
                    saved_revision: Some(0),
                    disk: loaded.disk,
                    read_only: loaded.read_only,
                })
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut doc = Self::from_text("");
                doc.path = Some(path.to_path_buf());
                Ok(doc)
            }
            Err(e) => Err(e),
        }
    }

    pub fn text(&self) -> &Rope {
        &self.text
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn set_path(&mut self, path: PathBuf) {
        self.path = Some(path);
        // A new name has never been saved with this content.
        self.saved_revision = None;
        self.disk = None;
    }

    pub fn display_name(&self) -> String {
        self.path
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "[scratch]".to_string())
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn version(&self) -> DocumentVersion {
        DocumentVersion(self.version)
    }

    pub fn read_only(&self) -> Option<ReadOnlyReason> {
        self.read_only
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    pub fn is_modified(&self) -> bool {
        self.pending.is_some() || self.saved_revision != Some(self.history.current())
    }

    /// Applies a transaction's changes. The caller passes the selection as it
    /// was before the edit so undo can restore it.
    pub fn apply(
        &mut self,
        tx: &Transaction,
        selection_before: &Selection,
    ) -> Result<(), EditError> {
        if let Some(r) = self.read_only {
            return Err(EditError::ReadOnly(r));
        }
        if tx.changes.len() != self.text.len_chars() {
            return Err(ChangeError::LengthMismatch {
                expected: tx.changes.len(),
                actual: self.text.len_chars(),
            }
            .into());
        }
        if tx.changes.is_empty() {
            return Ok(());
        }
        let inverse = tx.changes.invert(&self.text);
        tx.changes.apply(&mut self.text)?;
        self.version += 1;
        let pending = self.pending.get_or_insert_with(|| Pending {
            changes: Vec::new(),
            inverses: Vec::new(),
            selection_before: selection_before.clone(),
        });
        pending.changes.push(tx.changes.clone());
        pending.inverses.push(inverse);
        Ok(())
    }

    /// Applies a transaction only if the document is still at `expected`.
    pub fn apply_at_version(
        &mut self,
        tx: &Transaction,
        expected: DocumentVersion,
        selection_before: &Selection,
    ) -> Result<(), EditError> {
        if self.version() != expected {
            return Err(EditError::Stale {
                expected,
                actual: self.version(),
            });
        }
        self.apply(tx, selection_before)
    }

    /// Closes the current undo group. Returns true if a revision was created.
    pub fn commit(&mut self, selection_after: &Selection) -> bool {
        let Some(p) = self.pending.take() else {
            return false;
        };
        self.history.commit(
            p.changes,
            p.inverses,
            p.selection_before,
            selection_after.clone(),
        );
        true
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn undo(&mut self, current_selection: &Selection) -> Option<HistoryStep> {
        if self.read_only.is_some() {
            return None;
        }
        self.commit(current_selection);
        let rev = self.history.undo()?.clone();
        let mut applied = Vec::with_capacity(rev.inverses.len());
        for cs in rev.inverses.iter().rev() {
            cs.apply(&mut self.text)
                .expect("history inverse must match the document it was recorded against");
            self.version += 1;
            applied.push(cs.clone());
        }
        Some(HistoryStep {
            applied,
            selection: rev.selection_before,
        })
    }

    pub fn redo(&mut self, current_selection: &Selection) -> Option<HistoryStep> {
        if self.read_only.is_some() {
            return None;
        }
        self.commit(current_selection);
        let rev = self.history.redo()?.clone();
        let mut applied = Vec::with_capacity(rev.changes.len());
        for cs in &rev.changes {
            cs.apply(&mut self.text)
                .expect("history change must match the document it was recorded against");
            self.version += 1;
            applied.push(cs.clone());
        }
        Some(HistoryStep {
            applied,
            selection: rev.selection_after,
        })
    }

    /// Moves to any revision in the undo tree, chronologically numbered.
    /// Used by `earlier`/`later` to reach branches that plain redo cannot.
    pub fn goto_revision(
        &mut self,
        target: usize,
        current_selection: &Selection,
    ) -> Option<HistoryStep> {
        if self.read_only.is_some() {
            return None;
        }
        self.commit(current_selection);
        let (up, down) = self.history.path_to(target)?;
        if up.is_empty() && down.is_empty() {
            return None;
        }
        let mut applied = Vec::new();
        let mut selection = current_selection.clone();
        for id in &up {
            let rev = self.history.revision(*id).expect("path ids exist").clone();
            for cs in rev.inverses.iter().rev() {
                cs.apply(&mut self.text)
                    .expect("history inverse must match the document it was recorded against");
                self.version += 1;
                applied.push(cs.clone());
            }
            selection = rev.selection_before;
        }
        for id in &down {
            let rev = self.history.revision(*id).expect("path ids exist").clone();
            for cs in &rev.changes {
                cs.apply(&mut self.text)
                    .expect("history change must match the document it was recorded against");
                self.version += 1;
                applied.push(cs.clone());
            }
            selection = rev.selection_after;
        }
        self.history.arrive(target, &down);
        Some(HistoryStep { applied, selection })
    }

    /// True if the file on disk no longer matches what we last read or wrote,
    /// judged by metadata and file identity (cheap enough to poll).
    pub fn changed_on_disk(&self) -> bool {
        let Some(path) = &self.path else {
            return false;
        };
        match (self.disk, DiskState::read(path)) {
            (Some(then), Some(now)) => then.metadata_differs(&now),
            (Some(_), None) => true, // deleted or moved away
            // Never read from disk: a file appearing is handled by `save`.
            (None, _) => false,
        }
    }

    /// Saves to the document's path. Refuses if the file changed on disk
    /// since it was loaded or saved, unless `force` is set.
    pub fn save(&mut self, current_selection: &Selection, force: bool) -> Result<(), SaveError> {
        let path = self.path.clone().ok_or(SaveError::NoPath)?;
        if let Some(r) = self.read_only {
            return Err(SaveError::ReadOnly(r));
        }
        if !force && self.disk.is_some() && self.changed_on_disk() {
            return Err(SaveError::ExternallyModified(path));
        }
        // Metadata can be preserved by a rewrite (same size, timestamp put
        // back). Before overwriting, compare the actual bytes.
        if !force
            && let Some(expected) = self.disk.and_then(|d| d.hash)
            && path.exists()
            && crate::io::hash_file(&path)? != expected
        {
            return Err(SaveError::ExternallyModified(path));
        }
        if !force && self.disk.is_none() && path.exists() && self.saved_revision.is_some() {
            // We never read this file but something created it meanwhile.
            return Err(SaveError::ExternallyModified(path));
        }
        self.commit(current_selection);
        let disk = crate::io::save(&path, &self.text, self.encoding)?;
        self.disk = Some(disk);
        self.saved_revision = Some(self.history.current());
        Ok(())
    }

    /// Replaces the contents with the file on disk as one undoable change.
    /// Returns the change set applied so views can map their selections.
    pub fn reload(&mut self, current_selection: &Selection) -> io::Result<ChangeSet> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "document has no path"))?;
        let loaded = crate::io::load(&path)?;
        // Replacing editable text with a binary or undecodable file would
        // make the buffer read-only, and read-only buffers cannot undo, so
        // the previous text would be unreachable. Refuse instead.
        if let Some(reason @ (ReadOnlyReason::Binary | ReadOnlyReason::InvalidEncoding)) =
            loaded.read_only
            && self.read_only.is_none()
        {
            return Err(io::Error::other(format!(
                "not reloaded: the file on disk is now a {}",
                reason.describe()
            )));
        }
        self.commit(current_selection);
        let changes = differing_region(&self.text, &loaded.text);
        let was_read_only = self.read_only.take();
        let tx = Transaction::new(changes.clone());
        let result = self.apply(&tx, current_selection);
        self.read_only = loaded.read_only.or(match was_read_only {
            Some(ReadOnlyReason::Permission) => None,
            other => other,
        });
        result.map_err(|e| io::Error::other(e.to_string()))?;
        self.commit(&Selection::point(0));
        self.encoding = loaded.encoding;
        self.line_ending = LineEnding::detect(&self.text);
        self.disk = loaded.disk;
        self.saved_revision = Some(self.history.current());
        Ok(changes)
    }
}

/// A change set that turns `old` into `new` by replacing only the region
/// between their common prefix and common suffix. Positions outside that
/// region (cursors, marks, other views) keep their place across a reload,
/// and incremental consumers (syntax trees, language servers) see a small
/// edit instead of a whole-file replacement.
pub fn differing_region(old: &Rope, new: &Rope) -> ChangeSet {
    let (old_len, new_len) = (old.len_chars(), new.len_chars());
    let prefix = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let max_suffix = (old_len - prefix).min(new_len - prefix);
    let suffix = old
        .chars_at(old_len)
        .reversed()
        .zip(new.chars_at(new_len).reversed())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    let replacement = new.slice(prefix..new_len - suffix).to_string();
    ChangeSet::from_changes(
        old_len,
        [crate::change::Change::replace(
            prefix,
            old_len - suffix,
            replacement,
        )],
    )
    .expect("a single in-bounds replacement is always valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::Change;

    fn insert(doc: &mut Document, at: usize, s: &str) {
        let cs = ChangeSet::from_changes(doc.text().len_chars(), [Change::insert(at, s)]).unwrap();
        doc.apply(&Transaction::new(cs), &Selection::point(at))
            .unwrap();
    }

    #[test]
    fn undo_redo_and_branching() {
        let mut doc = Document::from_text("abc");
        insert(&mut doc, 3, "d");
        insert(&mut doc, 4, "e");
        doc.commit(&Selection::point(5));
        insert(&mut doc, 0, "X");
        doc.commit(&Selection::point(1));
        assert_eq!(doc.text().to_string(), "Xabcde");

        let step = doc.undo(&Selection::point(1)).unwrap();
        assert_eq!(doc.text().to_string(), "abcde");
        assert_eq!(step.selection, Selection::point(0));
        doc.undo(&Selection::point(0)).unwrap();
        assert_eq!(doc.text().to_string(), "abc");
        assert!(!doc.is_modified());
        assert!(doc.undo(&Selection::point(0)).is_none());

        doc.redo(&Selection::point(0)).unwrap();
        assert_eq!(doc.text().to_string(), "abcde");

        // A new edit starts a branch; redo now follows the new branch.
        insert(&mut doc, 0, "Y");
        doc.commit(&Selection::point(1));
        doc.undo(&Selection::point(1)).unwrap();
        doc.redo(&Selection::point(0)).unwrap();
        assert_eq!(doc.text().to_string(), "Yabcde");
        assert_eq!(doc.history().len(), 4);
    }

    #[test]
    fn versions_increase_and_stale_edits_are_refused() {
        let mut doc = Document::from_text("abc");
        let v0 = doc.version();
        insert(&mut doc, 0, "x");
        assert!(doc.version() > v0);
        let cs = ChangeSet::from_changes(4, [Change::delete(0, 1)]).unwrap();
        let err = doc
            .apply_at_version(&Transaction::new(cs.clone()), v0, &Selection::point(0))
            .unwrap_err();
        assert!(matches!(err, EditError::Stale { .. }));
        assert_eq!(doc.text().to_string(), "xabc");
        doc.apply_at_version(&Transaction::new(cs), doc.version(), &Selection::point(0))
            .unwrap();
        assert_eq!(doc.text().to_string(), "abc");
    }

    #[test]
    fn read_only_documents_refuse_edits() {
        let mut doc = Document::from_text("abc");
        doc.read_only = Some(ReadOnlyReason::Binary);
        let cs = ChangeSet::from_changes(3, [Change::delete(0, 1)]).unwrap();
        assert!(matches!(
            doc.apply(&Transaction::new(cs), &Selection::point(0)),
            Err(EditError::ReadOnly(_))
        ));
    }

    #[test]
    fn save_detects_external_modification() {
        let dir = std::env::temp_dir().join(format!("maestro-doc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ext.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut doc = Document::open(&path).unwrap();
        insert(&mut doc, 0, "zero ");
        assert!(doc.is_modified());
        // Someone else writes the file (different length so the check does
        // not depend on mtime resolution).
        std::fs::write(&path, "one\ntwo\n").unwrap();
        assert!(doc.changed_on_disk());
        assert!(matches!(
            doc.save(&Selection::point(0), false),
            Err(SaveError::ExternallyModified(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        doc.save(&Selection::point(0), true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "zero one\n");
        assert!(!doc.is_modified());

        // Reload picks up external content as an undoable change.
        std::fs::write(&path, "fresh\n").unwrap();
        doc.reload(&Selection::point(0)).unwrap();
        assert_eq!(doc.text().to_string(), "fresh\n");
        assert!(!doc.is_modified());
        doc.undo(&Selection::point(0)).unwrap();
        assert_eq!(doc.text().to_string(), "zero one\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn differing_region_replaces_only_what_changed() {
        let old = Rope::from_str("one\ntwo\nthree\n");
        let new = Rope::from_str("one\nTWO!\nthree\n");
        let cs = differing_region(&old, &new);
        assert_eq!(
            cs.changes(),
            vec![crate::change::Change::replace(4, 7, "TWO!")]
        );
        let mut t = old.clone();
        cs.apply(&mut t).unwrap();
        assert_eq!(t, new);
        // A position after the change keeps its place relative to the text.
        assert_eq!(cs.map_pos(10, crate::change::Assoc::After), 11);
        // Overlapping prefix and suffix ("aa" to "aaa") still round-trip.
        let (o, n) = (Rope::from_str("aa"), Rope::from_str("aaa"));
        let mut t = o.clone();
        differing_region(&o, &n).apply(&mut t).unwrap();
        assert_eq!(t, n);
        let same = differing_region(&n, &n);
        assert!(same.is_empty());
    }

    #[test]
    fn apply_rejects_mismatched_length_without_panicking() {
        let mut doc = Document::from_text("abc");
        let cs = ChangeSet::from_changes(10, [Change::delete(0, 10)]).unwrap();
        assert!(matches!(
            doc.apply(&Transaction::new(cs), &Selection::point(0)),
            Err(EditError::Change(ChangeError::LengthMismatch { .. }))
        ));
        assert_eq!(doc.text().to_string(), "abc");
    }

    #[test]
    fn earlier_and_later_reach_abandoned_branches() {
        let mut doc = Document::from_text("");
        let sel = Selection::point(0);
        insert(&mut doc, 0, "A");
        doc.commit(&sel); // revision 1
        doc.undo(&sel).unwrap();
        insert(&mut doc, 0, "B");
        doc.commit(&sel); // revision 2, a sibling of 1
        assert_eq!(doc.text().to_string(), "B");
        // Plain redo cannot reach "A"; revision navigation can.
        doc.goto_revision(1, &sel).unwrap();
        assert_eq!(doc.text().to_string(), "A");
        doc.goto_revision(2, &sel).unwrap();
        assert_eq!(doc.text().to_string(), "B");
        doc.goto_revision(0, &sel).unwrap();
        assert_eq!(doc.text().to_string(), "");
        // After arriving at 1 through navigation, redo follows that branch.
        doc.goto_revision(1, &sel).unwrap();
        doc.undo(&sel).unwrap();
        doc.redo(&sel).unwrap();
        assert_eq!(doc.text().to_string(), "A");
    }

    #[test]
    fn save_detects_same_size_rewrite_with_restored_mtime() {
        let dir = std::env::temp_dir().join(format!("maestro-doc-mtime-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        std::fs::write(&path, "aaaa\n").unwrap();
        let mut doc = Document::open(&path).unwrap();
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        insert(&mut doc, 0, "mine ");
        // Same length, different bytes, timestamp restored.
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        use std::io::Write;
        (&f).write_all(b"bbbb\n").unwrap();
        f.set_modified(mtime).unwrap();
        drop(f);
        assert!(matches!(
            doc.save(&Selection::point(0), false),
            Err(SaveError::ExternallyModified(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "bbbb\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reload_refuses_binary_replacement() {
        let dir = std::env::temp_dir().join(format!("maestro-doc-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let mut doc = Document::open(&path).unwrap();
        std::fs::write(&path, b"\x00\x01binary").unwrap();
        assert!(doc.reload(&Selection::point(0)).is_err());
        assert_eq!(doc.text().to_string(), "fn main() {}\n");
        assert!(doc.read_only().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_missing_file_creates_on_save() {
        let dir = std::env::temp_dir().join(format!("maestro-doc-new-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("new.txt");
        let mut doc = Document::open(&path).unwrap();
        assert_eq!(doc.text().len_chars(), 0);
        insert(&mut doc, 0, "hi\n");
        doc.save(&Selection::point(0), false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hi\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
