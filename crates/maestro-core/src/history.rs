//! Undo history as a revision tree.
//!
//! Every committed revision records the change sets that produced it, their
//! inverses, and the selections before and after. Undo walks to the parent;
//! redo walks to the most recently created child. Making a new edit after
//! undoing starts a new branch instead of discarding the old one, so no
//! committed state is ever lost. Navigating between branches by time is a
//! later feature; the data model already supports it.

use std::time::SystemTime;

use crate::change::ChangeSet;
use crate::selection::Selection;

#[derive(Debug, Clone)]
pub struct Revision {
    pub parent: usize,
    pub last_child: Option<usize>,
    /// Applied in order to move from the parent to this revision.
    pub changes: Vec<ChangeSet>,
    /// Applied in order to move from this revision back to the parent.
    pub inverses: Vec<ChangeSet>,
    pub selection_before: Selection,
    pub selection_after: Selection,
    pub timestamp: SystemTime,
}

#[derive(Debug, Clone)]
pub struct History {
    revisions: Vec<Revision>,
    current: usize,
}

impl Default for History {
    fn default() -> Self {
        Self {
            revisions: vec![Revision {
                parent: 0,
                last_child: None,
                changes: Vec::new(),
                inverses: Vec::new(),
                selection_before: Selection::point(0),
                selection_after: Selection::point(0),
                timestamp: SystemTime::now(),
            }],
            current: 0,
        }
    }
}

impl History {
    pub fn current(&self) -> usize {
        self.current
    }

    pub fn len(&self) -> usize {
        self.revisions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.revisions.len() <= 1
    }

    pub fn revision(&self, id: usize) -> Option<&Revision> {
        self.revisions.get(id)
    }

    /// Records a new revision as a child of the current one.
    pub fn commit(
        &mut self,
        changes: Vec<ChangeSet>,
        inverses: Vec<ChangeSet>,
        selection_before: Selection,
        selection_after: Selection,
    ) -> usize {
        let id = self.revisions.len();
        self.revisions.push(Revision {
            parent: self.current,
            last_child: None,
            changes,
            inverses,
            selection_before,
            selection_after,
            timestamp: SystemTime::now(),
        });
        self.revisions[self.current].last_child = Some(id);
        self.current = id;
        id
    }

    /// Steps to the parent and returns the revision being undone.
    pub fn undo(&mut self) -> Option<&Revision> {
        if self.current == 0 {
            return None;
        }
        let rev = self.current;
        self.current = self.revisions[rev].parent;
        Some(&self.revisions[rev])
    }

    /// Steps to the most recent child and returns it.
    pub fn redo(&mut self) -> Option<&Revision> {
        let child = self.revisions[self.current].last_child?;
        self.current = child;
        Some(&self.revisions[child])
    }

    pub fn can_undo(&self) -> bool {
        self.current != 0
    }

    pub fn can_redo(&self) -> bool {
        self.revisions[self.current].last_child.is_some()
    }
}
