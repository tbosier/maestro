//! Undo history as a revision tree.
//!
//! Every committed revision records the change sets that produced it, their
//! inverses, and the selections before and after. Undo walks to the parent;
//! redo walks to the most recently created child. Making a new edit after
//! undoing starts a new branch instead of discarding the old one, so no
//! committed state is ever lost. [`History::path_to`] reaches any revision,
//! which is how `earlier`/`later` (g- / g+) visit abandoned branches.

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

    /// The revisions to undo (in order) and then redo (in order) to move
    /// from the current revision to `target`. `None` if `target` is unknown.
    pub fn path_to(&self, target: usize) -> Option<(Vec<usize>, Vec<usize>)> {
        if target >= self.revisions.len() {
            return None;
        }
        let ancestors = |mut id: usize| {
            let mut v = vec![id];
            while id != 0 {
                id = self.revisions[id].parent;
                v.push(id);
            }
            v
        };
        let from_chain = ancestors(self.current);
        let to_chain = ancestors(target);
        let common = *from_chain
            .iter()
            .find(|id| to_chain.contains(id))
            .expect("the root is a common ancestor");
        let up: Vec<usize> = from_chain
            .into_iter()
            .take_while(|id| *id != common)
            .collect();
        let mut down: Vec<usize> = to_chain
            .into_iter()
            .take_while(|id| *id != common)
            .collect();
        down.reverse();
        Some((up, down))
    }

    /// Marks `target` current after walking `down`, pointing each parent's
    /// redo at the branch just taken.
    pub fn arrive(&mut self, target: usize, down: &[usize]) {
        for &id in down {
            let parent = self.revisions[id].parent;
            self.revisions[parent].last_child = Some(id);
        }
        self.current = target;
    }

    pub fn can_undo(&self) -> bool {
        self.current != 0
    }

    pub fn can_redo(&self) -> bool {
        self.revisions[self.current].last_child.is_some()
    }
}
