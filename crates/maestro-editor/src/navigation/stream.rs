//! Streams picker items from a background thread into an open picker.
//!
//! Two rules keep the editor responsive however fast the producer is:
//!
//! - At most one batch is queued at a time. While a batch waits to be
//!   applied, new items accumulate on the producer side.
//! - After a batch is applied, the next one waits at least [`GAP`]. The front
//!   end drains the job queue before it reads input, so this gap is what
//!   guarantees key presses are handled between batches.
//!
//! A batch that arrives after its picker was closed or replaced (the picker
//! id no longer matches) is dropped and sets the cancel flag, which the
//! producer checks to stop early.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::editor::{Editor, JobSender};
use crate::picker::PickerItem;

/// Minimum time between applying one batch and sending the next.
pub const GAP: Duration = Duration::from_millis(40);

pub struct Stream {
    sender: JobSender,
    picker_id: u64,
    cancel: Arc<AtomicBool>,
    in_flight: Arc<AtomicBool>,
    applied_at: Arc<Mutex<Instant>>,
    pending: Vec<PickerItem>,
    sent: usize,
}

impl Stream {
    pub fn new(sender: JobSender, picker_id: u64) -> Self {
        Self {
            sender,
            picker_id,
            cancel: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::new(AtomicBool::new(false)),
            // The first batch goes out as soon as there is something to show.
            applied_at: Arc::new(Mutex::new(
                Instant::now().checked_sub(GAP).unwrap_or_else(Instant::now),
            )),
            pending: Vec::new(),
            sent: 0,
        }
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Items pushed so far, sent or not.
    pub fn count(&self) -> usize {
        self.sent + self.pending.len()
    }

    /// Queues an item and sends a batch when the editor is ready for one.
    pub fn push(&mut self, item: PickerItem) {
        self.pending.push(item);
        self.maybe_send();
    }

    pub fn extend(&mut self, items: impl IntoIterator<Item = PickerItem>) {
        self.pending.extend(items);
        self.maybe_send();
    }

    /// Sends pending items if the editor is ready. Producers that go a
    /// while between items call this so earlier items are not held back.
    pub fn tick(&mut self) {
        self.maybe_send();
    }

    fn ready(&self) -> bool {
        !self.in_flight.load(Ordering::Acquire)
            && self
                .applied_at
                .lock()
                .map(|t| t.elapsed() >= GAP)
                .unwrap_or(true)
    }

    fn maybe_send(&mut self) {
        if !self.pending.is_empty() && self.ready() {
            self.send(None);
        }
    }

    /// Sends what is pending. `done` carries the final title (or `None` to
    /// keep it) and ends loading.
    fn send(&mut self, done: Option<Option<String>>) {
        let items = std::mem::take(&mut self.pending);
        self.sent += items.len();
        let id = self.picker_id;
        let cancel = self.cancel.clone();
        let in_flight = self.in_flight.clone();
        let applied_at = self.applied_at.clone();
        in_flight.store(true, Ordering::Release);
        let ok = self.sender.send(move |ed: &mut Editor| {
            match ed.picker.as_mut() {
                Some(p) if p.id == id => {
                    p.extend(items);
                    if let Some(title) = done {
                        p.loading = false;
                        if let Some(t) = title {
                            p.title = t;
                        }
                    }
                    super::preview::refresh(ed);
                }
                _ => cancel.store(true, Ordering::Relaxed),
            }
            if let Ok(mut t) = applied_at.lock() {
                *t = Instant::now();
            }
            in_flight.store(false, Ordering::Release);
        });
        if !ok {
            self.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Sends the remaining items and marks the picker as done loading,
    /// optionally replacing its title.
    pub fn finish(mut self, title: Option<String>) {
        if !self.cancelled() {
            self.send(Some(title));
        }
    }
}
