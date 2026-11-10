//! The shell pane (`space o`, `:term`): one shell in a terminal, docked
//! under the views, above the status line. The sidebar and the file tree
//! keep their columns; the pane takes rows from the views between them.
//!
//! * `space o` shows the pane and gives the shell the keys, starting
//!   `$SHELL` (else `/bin/sh`) in the editor's working directory when no
//!   shell runs. Pressed again from the editor, it hides the pane; the
//!   shell keeps running.
//! * While the shell has the keys every key goes to it, and pastes too.
//!   `C-\` gives the keys back with the pane still shown, and `C-\` from
//!   the editor gives them to the shown shell again. A click in the pane
//!   gives it the keys and a click outside takes them back; the wheel
//!   scrolls its scrollback.
//! * When the shell exits, the pane says how and the keys come back. The
//!   next `space o` starts a new shell in the working directory of that
//!   moment. Switching worktrees leaves a running shell where it is; the
//!   header names the directory it started in.
//! * Quitting the editor, or dropping it, ends the shell, its jobs and
//!   what they started, the way terminal agents end (see
//!   `maestro_term::Terminal::kill`: the process group and the session).
//!
//! Why `space o` hides a shown shell instead of focusing it, as `space t`
//! does for the file tree: while the shell has the keys, `space` goes to
//! the shell, so a toggle that only hid a focused pane could never be
//! reached from the keyboard. `C-\` moves the keys both ways instead.
//!
//! The editor owns the split: [`Editor::resize`] takes the rows and
//! columns the views and the pane share (its *region*) and gives the views
//! what the pane leaves. When the region is too short for both, the pane
//! is not drawn and takes no keys.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use maestro_term::{CommandSpec, ExitInfo, TermSize, Terminal};

use crate::editor::{Editor, MouseKind, Severity};
use crate::key::KeyEvent;
use crate::layout::Rect;
use crate::term_input::{is_toggle, send_key, send_paste};

crate::commands! {
    toggle_shell => "Show the shell pane under the views and type into it, or hide it",
}

/// Default key bindings, added to the `space` group.
pub const DEFAULT_KEYS: &[(&str, &str)] = &[("space o", "toggle_shell")];

/// The pane's fewest rows: its header and five rows of terminal.
pub const MIN_ROWS: u16 = 6;
/// Rows the views keep under the pane's rule: with fewer, the pane is not
/// drawn.
pub const VIEWS_MIN_ROWS: u16 = 8;
/// The shell when `$SHELL` is unset or empty.
const FALLBACK_SHELL: &str = "/bin/sh";
/// How long quitting waits for the shell's process group to end: its
/// SIGKILL comes 200 ms after SIGHUP, on a thread that dies with the
/// editor's process.
const STOP_WAIT: Duration = Duration::from_secs(1);

/// The shell pane's state, kept on the editor as `ed.shell`.
#[derive(Default)]
pub struct ShellPane {
    /// The pane is shown (when the region has room for it).
    pub open: bool,
    /// The shell has the keys (while the pane is shown and the shell runs).
    pub focused: bool,
    /// The program to run instead of `$SHELL`, with no arguments. Tests set
    /// it so they do not depend on the user's shell.
    pub program: Option<String>,
    /// The rows and columns the views and the pane share; see
    /// [`Editor::resize`].
    pub(crate) region: Rect,
    session: Option<Session>,
}

/// One shell process and what the pane knows about it.
pub struct Session {
    pub terminal: Arc<Terminal>,
    /// The directory it started in. It does not follow worktree switches.
    pub dir: PathBuf,
    /// Set once the shell has exited.
    pub exited: Option<ExitInfo>,
    /// Counts output chunks, bumped on the terminal's reader thread.
    output: Arc<AtomicU64>,
    seen: u64,
    size: TermSize,
}

impl ShellPane {
    /// The shell, running or exited, if one was started.
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// True while a started shell has not exited.
    pub fn running(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.terminal.exit_status().is_none())
    }

    /// The pane's rows, when it is open and the region has room for it.
    pub fn area(&self) -> Option<Rect> {
        if self.open { room(self.region) } else { None }
    }

    /// True when keys go to the shell: it has them, it runs, and the pane
    /// is drawn.
    pub fn has_focus(&self) -> bool {
        self.focused && self.area().is_some() && self.running()
    }
}

impl Drop for ShellPane {
    /// Ends the shell and what it started, whatever ends the editor, and
    /// waits a moment for it (see [`STOP_WAIT`]).
    fn drop(&mut self) {
        stop_session(self.session.as_ref());
    }
}

/// Where the pane goes in `region`, open or not: the bottom third, at
/// least [`MIN_ROWS`], leaving the views [`VIEWS_MIN_ROWS`]. `None` when
/// the region is too short for both or too narrow for a terminal.
pub fn room(region: Rect) -> Option<Rect> {
    if region.height < MIN_ROWS + VIEWS_MIN_ROWS || region.width < 10 {
        return None;
    }
    let h = (region.height / 3)
        .max(MIN_ROWS)
        .min(region.height - VIEWS_MIN_ROWS);
    Some(Rect::new(region.x, region.bottom() - h, region.width, h))
}

/// The views' part of `region` when the pane takes `pane`.
pub(crate) fn views_area(region: Rect, pane: Option<Rect>) -> Rect {
    match pane {
        Some(p) => Rect::new(region.x, region.y, region.width, p.y - region.y),
        None => region,
    }
}

/// The terminal's part of the pane: everything under the header.
pub fn screen_area(pane: Rect) -> Rect {
    Rect::new(
        pane.x,
        pane.y + 1.min(pane.height),
        pane.width,
        pane.height.saturating_sub(1),
    )
}

fn term_size(pane: Rect) -> TermSize {
    let s = screen_area(pane);
    TermSize {
        rows: s.height.max(1),
        cols: s.width.max(2),
    }
}

/// What the pane's header says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The directory the shell started in, from the home directory
    /// (`~/proj`). Empty before a shell starts.
    pub dir: String,
    /// Who has the keys, or how the shell exited: longest first, so a
    /// narrow pane can take a shorter one.
    pub badges: Vec<String>,
    /// True while the shell has the keys.
    pub typing: bool,
}

/// The pane's header: `shell  ~/proj` and who has the keys.
pub fn header(ed: &Editor) -> Header {
    let s = &ed.shell;
    let Some(session) = s.session() else {
        return Header {
            dir: String::new(),
            badges: vec![" not started ".into()],
            typing: false,
        };
    };
    let dir = crate::navigation::home_path(&session.dir);
    let badges = if let Some(exit) = &session.exited {
        vec![
            format!(" {}; space o starts a new shell ", exit.description),
            format!(" {} ", exit.description),
            " exited ".into(),
        ]
    } else if s.has_focus() {
        vec![
            " KEYS: TYPING INTO SHELL (C-\\ or a click outside returns) ".into(),
            " KEYS: TYPING INTO SHELL (C-\\ returns) ".into(),
            " KEYS: SHELL ".into(),
            " SHELL ".into(),
        ]
    } else if ed.agents.panel_open() {
        // The agent panel keeps C-\ for its own terminals.
        vec![
            " KEYS: AGENT PANEL (a click types into the shell) ".into(),
            " KEYS: AGENT PANEL ".into(),
            " PANEL ".into(),
        ]
    } else {
        vec![
            " KEYS: EDITOR (C-\\ or a click types into the shell; space o hides it) ".into(),
            " KEYS: EDITOR (C-\\ types into the shell) ".into(),
            " KEYS: EDITOR ".into(),
            " EDITOR ".into(),
        ]
    };
    Header {
        dir,
        badges,
        typing: s.has_focus(),
    }
}

fn toggle_shell(ed: &mut Editor) {
    toggle(ed);
}

/// `space o` and `:term`: shows the pane and gives the shell the keys,
/// starting a shell when none runs; hides the pane when it is shown with a
/// running shell. Hiding leaves the shell running.
pub fn toggle(ed: &mut Editor) {
    if ed.shell.open && ed.shell.running() {
        ed.shell.open = false;
        ed.shell.focused = false;
        ed.resize(ed.region());
        ed.info("the shell pane is hidden; the shell keeps running (space o shows it)");
        return;
    }
    if room(ed.region()).is_none() {
        ed.set_status(
            Severity::Warning,
            format!(
                "the window is too small for the shell pane: it needs {} rows for the views and the pane",
                MIN_ROWS + VIEWS_MIN_ROWS
            ),
        );
        return;
    }
    ed.shell.open = true;
    ed.resize(ed.region());
    if !ed.shell.running() && !start(ed) {
        ed.shell.open = false;
        ed.resize(ed.region());
        return;
    }
    focus(ed);
}

/// Starts a new shell in the working directory, replacing an exited one.
/// Reports a failure and returns false.
fn start(ed: &mut Editor) -> bool {
    let Some(pane) = ed.shell.area() else {
        return false;
    };
    let program = ed.shell.program.clone().unwrap_or_else(|| {
        std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| FALLBACK_SHELL.into())
    });
    let dir = ed.cwd.clone();
    let size = term_size(pane);
    let output = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&output);
    let wake = ed.wake_handle();
    let spec = CommandSpec {
        program: program.clone(),
        args: Vec::new(),
        cwd: dir.clone(),
        env: Vec::new(),
    };
    let started = Terminal::spawn(&spec, size, move || {
        counter.fetch_add(1, Ordering::SeqCst);
        wake.wake();
    });
    match started {
        Ok(t) => {
            // An exited shell's terminal goes now; it has nothing to stop.
            ed.shell.session = Some(Session {
                terminal: Arc::new(t),
                dir,
                exited: None,
                output,
                seen: 0,
                size,
            });
            true
        }
        Err(e) => {
            ed.error(format!(
                "could not start the shell {}: {e}",
                crate::navigation::home_path(std::path::Path::new(&program))
            ));
            false
        }
    }
}

/// Gives the shell the keys, taking them from the sidebar, the file tree
/// and an agent's terminal.
pub(crate) fn focus(ed: &mut Editor) {
    ed.hand_keys_to_terminal();
    if let Some(s) = &ed.shell.session {
        s.terminal.scroll(0);
    }
    ed.shell.focused = true;
    ed.workspaces.sidebar.focused = false;
    crate::workspaces::tree::blur(ed);
    ed.agents.panel.terminal_focus = None;
    ed.info("you are typing into the shell; C-\\ or a click outside it takes the keys back");
}

fn blur(ed: &mut Editor) {
    ed.shell.focused = false;
    ed.info("keys are back with the editor; the shell keeps running (C-\\ types into it again)");
}

/// True when `key` should give the keys to the shown shell: `C-\` with the
/// pane shown and its shell running. The agent panel keeps `C-\` for its
/// own terminals.
pub(crate) fn takes_keys_on(ed: &Editor, key: KeyEvent) -> bool {
    is_toggle(key)
        && !ed.shell.focused
        && ed.shell.area().is_some()
        && ed.shell.running()
        && !ed.agents.panel_open()
}

/// A key while the shell has the keys: `C-\` gives them back, anything
/// else goes to the shell.
pub(crate) fn handle_key(ed: &mut Editor, key: KeyEvent) {
    if is_toggle(key) {
        blur(ed);
        return;
    }
    let Some(t) = ed.shell.session.as_ref().map(|s| Arc::clone(&s.terminal)) else {
        ed.shell.focused = false;
        return;
    };
    if let Err(e) = send_key(&t, key) {
        ed.shell.focused = false;
        ed.error(format!(
            "the shell does not take input ({e}); keys are back with the editor"
        ));
    }
}

/// Pasted text while the shell has the keys.
pub(crate) fn paste(ed: &mut Editor, text: &str) {
    let Some(t) = ed.shell.session.as_ref().map(|s| Arc::clone(&s.terminal)) else {
        return;
    };
    if let Err(e) = send_paste(&t, text) {
        ed.error(format!("could not paste into the shell: {e}"));
    }
}

/// A mouse event at screen cell (x, y). Returns true when the pane took
/// it. A click elsewhere takes the keys back from the shell and returns
/// false, so the rest of the screen handles it.
pub(crate) fn handle_mouse(ed: &mut Editor, kind: MouseKind, x: u16, y: u16) -> bool {
    let Some(pane) = ed.shell.area() else {
        return false;
    };
    if !pane.contains(x, y) {
        if kind == MouseKind::Down && ed.shell.focused {
            blur(ed);
        }
        return false;
    }
    let Some(t) = ed.shell.session.as_ref().map(|s| Arc::clone(&s.terminal)) else {
        return true;
    };
    match kind {
        MouseKind::ScrollUp => t.scroll(3),
        MouseKind::ScrollDown => t.scroll(-3),
        MouseKind::Drag => {}
        MouseKind::Down => {
            if ed.shell.running() {
                if !ed.shell.focused {
                    focus(ed);
                }
            } else {
                ed.info("the shell has exited; space o starts a new one");
            }
        }
    }
    true
}

/// Sizes the shell's terminal to the pane. Called after every resize and
/// from [`poll`]; does nothing while the pane is hidden.
pub(crate) fn fit(ed: &mut Editor) {
    let Some(pane) = ed.shell.area() else {
        return;
    };
    let size = term_size(pane);
    if let Some(s) = &mut ed.shell.session
        && s.size != size
    {
        let _ = s.terminal.resize(size);
        s.size = size;
    }
}

/// Notices the shell's output and its exit, as [`crate::agents::poll`]
/// does for terminal agents. Returns true when the screen needs drawing.
pub(crate) fn poll(ed: &mut Editor) -> bool {
    fit(ed);
    let shown = ed.shell.area().is_some();
    let Some(s) = &mut ed.shell.session else {
        return false;
    };
    let mut changed = false;
    let n = s.output.load(Ordering::SeqCst);
    if n != s.seen {
        s.seen = n;
        changed = shown;
    }
    if s.exited.is_none()
        && let Some(info) = s.terminal.exit_status()
    {
        let msg = format!("the shell {}", info.description);
        s.exited = Some(info);
        if std::mem::take(&mut ed.shell.focused) {
            ed.info(format!(
                "{msg}; keys are back with the editor (space o starts a new shell)"
            ));
        } else {
            ed.info(format!("{msg} (space o starts a new shell)"));
        }
        changed = true;
    }
    changed
}

/// Ends the shell for quitting and waits a moment for its process group
/// to go (see [`STOP_WAIT`]).
pub(crate) fn stop(ed: &mut Editor) {
    stop_session(ed.shell.session.as_ref());
}

fn stop_session(session: Option<&Session>) {
    let Some(s) = session else {
        return;
    };
    if s.terminal.exit_status().is_some() {
        return;
    }
    let _ = s.terminal.kill();
    let deadline = Instant::now() + STOP_WAIT;
    while Instant::now() < deadline && s.terminal.exit_status().is_none() {
        std::thread::sleep(Duration::from_millis(10));
    }
}
