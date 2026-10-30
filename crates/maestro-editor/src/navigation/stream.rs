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
//! The final batch follows the same rules. A batch that arrives after its
//! picker was closed or replaced (the picker id no longer matches) is
//! dropped and sets the cancel flag, which the producer checks to stop
//! early. A producer with nothing to send still sends an empty probe every
//! [`PROBE`], so a search that finds nothing notices a closed picker too.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::editor::{Editor, JobSender};
use crate::picker::PickerItem;

/// Minimum time between applying one batch and sending the next.
pub const GAP: Duration = Duration::from_millis(40);
/// How often an idle producer checks that its picker is still open.
pub const PROBE: Duration = Duration::from_millis(250);
/// The final batch waits at most this long for the editor to take the
/// previous one (it may have stopped polling).
const FINISH_WAIT: Duration = Duration::from_secs(5);

pub struct Stream {
    sender: JobSender,
    picker_id: u64,
    cancel: Arc<AtomicBool>,
    in_flight: Arc<AtomicBool>,
    applied_at: Arc<Mutex<Instant>>,
    pending: Vec<PickerItem>,
    sent: usize,
    last_send: Instant,
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
            last_send: Instant::now(),
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

    /// Sends pending items if the editor is ready, or an empty probe if
    /// nothing was sent for [`PROBE`]. Producers call this regularly so
    /// earlier items are not held back and a closed picker is noticed.
    pub fn tick(&mut self) {
        if self.pending.is_empty() {
            if self.last_send.elapsed() >= PROBE && self.ready() {
                self.send(None);
            }
        } else {
            self.maybe_send();
        }
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
        self.last_send = Instant::now();
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
        let start = Instant::now();
        while !self.ready() && !self.cancelled() && start.elapsed() < FINISH_WAIT {
            std::thread::sleep(Duration::from_millis(2));
        }
        if !self.cancelled() {
            self.send(Some(title));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Rect;
    use crate::picker::{Picker, PickerAction};
    use maestro_config::Config;

    fn item(s: &str) -> PickerItem {
        PickerItem {
            label: s.into(),
            detail: String::new(),
            action: PickerAction::Command(s.into()),
        }
    }

    fn poll_until(ed: &mut Editor, mut done: impl FnMut(&Editor) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(ed) {
            assert!(Instant::now() < deadline, "timed out");
            ed.poll_background();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn an_idle_producer_notices_its_picker_closed() {
        let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 80, 24));
        let (tx, rx) = std::sync::mpsc::channel();
        // No picker has this id, as after the user closed it.
        ed.spawn_background("probe-test", move |sender| {
            let mut s = Stream::new(sender, u64::MAX);
            let t = Instant::now();
            while !s.cancelled() && t.elapsed() < Duration::from_secs(5) {
                s.tick();
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = tx.send((s.cancelled(), t.elapsed()));
        })
        .unwrap();
        poll_until(&mut ed, |e| !e.has_background_work());
        let (cancelled, took) = rx.recv().unwrap();
        assert!(cancelled);
        assert!(took < Duration::from_secs(2), "{took:?}");
    }

    #[test]
    fn the_final_batch_waits_for_the_previous_one() {
        let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 80, 24));
        let mut picker = Picker::new("t", Vec::new());
        picker.loading = true;
        let id = picker.id;
        ed.picker = Some(picker);
        ed.spawn_background("finish-test", move |sender| {
            let mut s = Stream::new(sender, id);
            s.push(item("first"));
            s.push(item("second"));
            s.finish(None);
        })
        .unwrap();
        // Nothing is applied yet, so the producer must still be waiting
        // with the second item rather than queueing it behind the first.
        std::thread::sleep(Duration::from_millis(100));
        assert!(ed.has_background_work());
        assert!(ed.poll_background());
        assert_eq!(ed.picker.as_ref().unwrap().total(), 1);
        poll_until(&mut ed, |e| !e.picker.as_ref().unwrap().loading);
        assert_eq!(ed.picker.as_ref().unwrap().total(), 2);
    }
}
