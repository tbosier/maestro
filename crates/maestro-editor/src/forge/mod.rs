//! The GitHub inbox (`space i`, `:inbox`): pull requests and issues waiting
//! on the user, a pull request's details, checks and diff, and the write
//! actions (approve, comment, request changes, merge), all through the
//! GitHub CLI by way of `maestro-forge`.
//!
//! Every `gh` call runs on a background thread. Nothing is shown that gh
//! did not return: a failed query says why in its section, and when gh is
//! missing or logged out the screen says that and how to fix it.

pub mod actions;
pub mod detail;
pub mod screen;

use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use maestro_forge::{Gh, Inbox};

/// The GitHub types the inbox holds, for front ends.
pub use maestro_forge as api;

use crate::editor::{DocumentId, Editor};

crate::commands! {
    github_inbox => "Show the GitHub inbox: pull requests and issues waiting on you",
}

/// Default key bindings, added to the `space` group.
pub const DEFAULT_KEYS: &[(&str, &str)] = &[("space i", "github_inbox")];

fn github_inbox(ed: &mut Editor) {
    screen::open(ed);
}

/// A comment or change request being written in a buffer of its own,
/// sent with `:pr-send` after a confirmation that shows it in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDraft {
    pub doc: DocumentId,
    pub pr: maestro_forge::PrRef,
    pub kind: DraftKind,
    /// For a change request: the head commit shown when it was started,
    /// which the review is submitted on.
    pub head: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftKind {
    Comment,
    RequestChanges,
}

impl DraftKind {
    pub fn label(self) -> &'static str {
        match self {
            DraftKind::Comment => "comment",
            DraftKind::RequestChanges => "change request",
        }
    }
}

/// The inbox's state on the editor.
#[derive(Default)]
pub struct Forge {
    /// The last load, kept while a new one runs.
    pub inbox: Option<Inbox>,
    pub loading: bool,
    /// A refresh was asked for while one ran; it runs when that ends.
    again: bool,
    /// When the last load finished: Unix seconds and the instant.
    pub refreshed: Option<(i64, Instant)>,
    /// Whether the screen is shown. Its state is kept while hidden, so
    /// `space i` returns to the same place.
    pub open: bool,
    pub screen: screen::Screen,
    /// Messages being written in buffers.
    pub drafts: Vec<MessageDraft>,
    /// A write action being sent: what it is, for the screen.
    pub writing: Option<String>,
    /// Numbers detail loads across detail views, so a slow answer for an
    /// earlier one is dropped even after the view was closed and reopened.
    pub(crate) detail_seq: u64,
}

impl Forge {
    pub fn screen_open(&self) -> bool {
        self.open
    }
}

/// The configured `gh` program.
pub fn gh(ed: &Editor) -> Gh {
    let program: PathBuf = crate::editor::expand_tilde(std::path::Path::new(&ed.config.github.gh));
    Gh::new(program)
}

/// Now, in Unix seconds.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Open pull requests waiting for the user's review, as of the last
/// refresh. `None` before the first load and when it failed. The status
/// line shows a badge when this is above zero.
pub fn review_requests(ed: &Editor) -> Option<usize> {
    ed.forge.inbox.as_ref()?.review_requests()
}

/// Reloads the inbox in the background. A refresh asked for while one runs
/// starts when that one ends.
pub fn refresh(ed: &mut Editor) {
    if ed.forge.loading {
        ed.forge.again = true;
        return;
    }
    let gh = gh(ed);
    let cwd = ed.cwd.clone();
    ed.forge.loading = true;
    ed.forge.again = false;
    let spawned = ed.spawn_background("github-inbox", move |tx| {
        let inbox = maestro_forge::load_inbox(&gh, &cwd, maestro_forge::DEFAULT_LIMIT);
        tx.send(move |ed| loaded(ed, inbox));
    });
    if let Err(e) = spawned {
        ed.forge.loading = false;
        ed.error(format!("could not start loading the GitHub inbox: {e}"));
    }
}

fn loaded(ed: &mut Editor, inbox: Inbox) {
    ed.forge.loading = false;
    ed.forge.refreshed = Some((now_unix(), Instant::now()));
    ed.forge.inbox = Some(inbox);
    screen::after_load(ed);
    if ed.forge.again {
        refresh(ed);
    }
}

/// Called about once a second: reloads on the configured interval
/// (`[github] refresh-secs`; 0 turns it off).
pub fn tick(ed: &mut Editor, now: Instant) {
    let secs = ed.config.github.refresh_secs;
    if secs == 0 || ed.forge.loading {
        return;
    }
    let due = match ed.forge.refreshed {
        None => true,
        Some((_, at)) => now.saturating_duration_since(at).as_secs() >= secs,
    };
    if due {
        refresh(ed);
    }
}
