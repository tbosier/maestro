//! Editor state and key dispatch.
//!
//! The [`Editor`] owns buffers, views, modes, registers and the keymap. It is
//! driven by [`Editor::handle_key`] and read by front ends; it never touches a
//! terminal. Tests drive it with key sequences exactly as a user would.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use maestro_config::{Config, LineNumbers, Theme};
use maestro_core::document::EditError;
use maestro_core::{Document, DocumentVersion, Range, Selection, Transaction};

use crate::key::{KeyCode, KeyEvent};
use crate::keymap::{KeyTrie, Keymaps, Lookup};
use crate::layout::{Direction, Layout, Rect};
use crate::picker::Picker;
use crate::prompt::Prompt;
use crate::registers::{Clipboard, MemoryClipboard, Registers};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocumentId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViewId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    /// Normal mode where motions extend selections instead of replacing them.
    Select,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Select => "SELECT",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Down,
    Drag,
    ScrollUp,
    ScrollDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// How much interface is shown. Switching never changes editor state, only
/// presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Presentation {
    /// Text only: no tabline, no gutter, quiet status line.
    Focus,
    /// The normal editing interface.
    #[default]
    Engineering,
    /// Engineering plus agent and worktree panels (when those exist).
    Swarm,
}

pub struct Buffer {
    pub id: DocumentId,
    pub doc: Document,
    /// Selection to restore when a view switches back to this buffer.
    pub last_selection: Selection,
    /// The version last written to the recovery directory.
    pub recovery_version: Option<DocumentVersion>,
    /// When the buffer last changed, for idle-triggered recovery writes.
    pub last_edit: Option<Instant>,
    /// Set once the user has been told the file changed on disk.
    pub disk_change_reported: bool,
    /// Language, syntax tree and structural-selection state.
    pub syntax: crate::syntax::BufferSyntax,
}

#[derive(Debug, Clone)]
pub struct View {
    pub id: ViewId,
    pub doc: DocumentId,
    pub selection: Selection,
    pub area: Rect,
    pub top_line: usize,
    pub left_col: usize,
    /// Remembered display column per range for vertical motion.
    pub sticky_cols: Option<Vec<usize>>,
    pub jumps: Vec<(DocumentId, Selection)>,
    pub jump_index: usize,
    /// The previously shown document, for `ga` (alternate file).
    pub alternate: Option<DocumentId>,
}

impl View {
    /// Rows available for text (the last row is the status line).
    pub fn text_height(&self) -> usize {
        self.area.height.saturating_sub(1) as usize
    }
}

type CharCallback = Box<dyn FnOnce(&mut Editor, char)>;

/// Work finished on another thread, applied to the editor on the main
/// thread by [`Editor::poll_background`].
pub type Job = Box<dyn FnOnce(&mut Editor) + Send>;

/// Wakes the front end's event loop from another thread. The front end
/// installs the callback once with [`Editor::set_waker`]; until then (and in
/// tests, which poll) waking does nothing. Handles made before the callback
/// was installed still reach it.
#[derive(Clone, Default)]
pub struct WakeHandle(std::sync::Arc<std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>>);

impl WakeHandle {
    pub fn wake(&self) {
        if let Some(f) = self.0.get() {
            f();
        }
    }
}

impl std::fmt::Debug for WakeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WakeHandle")
    }
}

/// Handle a background thread uses to send results back to the editor.
/// Each send wakes the front end, so results appear without waiting for
/// the next key press or tick.
#[derive(Clone)]
pub struct JobSender {
    tx: std::sync::mpsc::Sender<Job>,
    wake: WakeHandle,
    _active: std::sync::Arc<ActiveGuard>,
}

/// Counts a live background task; `None` for long-lived idle senders.
struct ActiveGuard(Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        if let Some(c) = &self.0 {
            c.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

impl JobSender {
    /// Queues `job` to run on the editor. Returns false if the editor is gone.
    pub fn send(&self, job: impl FnOnce(&mut Editor) + Send + 'static) -> bool {
        let sent = self.tx.send(Box::new(job)).is_ok();
        if sent {
            self.wake.wake();
        }
        sent
    }
}

pub struct Editor {
    pub buffers: BTreeMap<DocumentId, Buffer>,
    pub views: BTreeMap<ViewId, View>,
    pub layout: Layout,
    pub focus: ViewId,
    pub mode: Mode,
    pub presentation: Presentation,
    pub config: Config,
    pub theme: Theme,
    pub registers: Registers,
    pub status: Option<(Severity, String)>,
    pub prompt: Option<Prompt>,
    pub picker: Option<Picker>,
    pub keymaps: Keymaps,
    /// Keys typed so far in an incomplete multi-key binding.
    pub pending: Vec<KeyEvent>,
    pub count: Option<usize>,
    pub selected_register: Option<char>,
    pending_char: Option<(String, CharCallback)>,
    /// Set after `"`: the next character names the register for the
    /// following command.
    awaiting_register: bool,
    pub last_search: Option<String>,
    pub last_find: Option<(char, bool, bool)>,
    /// Macro being recorded: target register and keys so far.
    pub recording: Option<(char, Vec<KeyEvent>)>,
    /// Keys of the last change, replayed by `.`.
    pub last_change: Vec<KeyEvent>,
    change_keys: Vec<KeyEvent>,
    in_insert_change: bool,
    /// Set during a key whose edit cannot be repeated by replaying keys
    /// (see [`Editor::not_repeatable`]).
    unrepeatable: bool,
    replaying: bool,
    pub marks: HashMap<char, (DocumentId, usize)>,
    pub area: Rect,
    pub should_quit: bool,
    pub cwd: PathBuf,
    pub clipboard: Box<dyn Clipboard>,
    pub recovery_dir: Option<PathBuf>,
    /// Identifies this editor's recovery files (see `recovery`).
    pub session_id: String,
    /// Nesting depth of macro replays, to stop self-invoking macros.
    pub macro_depth: usize,
    /// Gutter signs and branch names; see [`crate::git`].
    pub git: crate::git::GitState,
    /// Language servers, synced documents and diagnostics.
    pub lsp: crate::lsp::Lsp,
    /// Sessions, recent files and the state they are stored in.
    pub navigation: crate::navigation::NavState,
    /// Coding agents and the agent panel (see [`crate::agents`]).
    pub agents: crate::agents::AgentState,
    /// Edit proposals from automation clients and their review overlay
    /// (see [`crate::proposals`]).
    pub proposals: crate::proposals::Proposals,
    /// Projects, worktrees, the sidebar, agent attention and switching
    /// between worktrees (see [`crate::workspaces`]).
    pub workspaces: crate::workspaces::Workspaces,
    /// The GitHub inbox (see [`crate::forge`]).
    pub forge: crate::forge::Forge,
    jobs_tx: std::sync::mpsc::Sender<Job>,
    jobs_rx: std::sync::mpsc::Receiver<Job>,
    active_jobs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    wake: WakeHandle,
    next_id: u64,
}

impl Editor {
    pub fn new(config: Config, area: Rect) -> Self {
        let keymaps = Keymaps::for_config(&config);
        let theme = Theme::duskfox();
        let (jobs_tx, jobs_rx) = std::sync::mpsc::channel();
        let mut editor = Self {
            buffers: BTreeMap::new(),
            views: BTreeMap::new(),
            layout: Layout::new(ViewId(0)),
            focus: ViewId(0),
            mode: Mode::Normal,
            presentation: Presentation::default(),
            config,
            theme,
            registers: Registers::default(),
            status: None,
            prompt: None,
            picker: None,
            keymaps,
            pending: Vec::new(),
            count: None,
            selected_register: None,
            pending_char: None,
            awaiting_register: false,
            last_search: None,
            last_find: None,
            recording: None,
            last_change: Vec::new(),
            change_keys: Vec::new(),
            in_insert_change: false,
            unrepeatable: false,
            replaying: false,
            marks: HashMap::new(),
            area,
            should_quit: false,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            clipboard: Box::new(MemoryClipboard::default()),
            recovery_dir: None,
            session_id: crate::recovery::new_session_id(),
            macro_depth: 0,
            git: crate::git::GitState::default(),
            lsp: Default::default(),
            navigation: Default::default(),
            agents: Default::default(),
            proposals: Default::default(),
            workspaces: Default::default(),
            forge: Default::default(),
            jobs_tx,
            jobs_rx,
            active_jobs: Default::default(),
            wake: WakeHandle::default(),
            next_id: 1,
        };
        let doc = editor.add_document(Document::default());
        let view = editor.new_view(doc);
        editor.layout = Layout::new(view);
        editor.focus = view;
        editor.relayout();
        editor
    }

    fn fresh_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn add_document(&mut self, doc: Document) -> DocumentId {
        let id = DocumentId(self.fresh_id());
        self.buffers.insert(
            id,
            Buffer {
                id,
                doc,
                last_selection: Selection::point(0),
                recovery_version: None,
                last_edit: None,
                disk_change_reported: false,
                syntax: Default::default(),
            },
        );
        self.refresh_syntax(id);
        id
    }

    pub(crate) fn new_view(&mut self, doc: DocumentId) -> ViewId {
        let id = ViewId(self.fresh_id());
        let selection = self.buffers[&doc].last_selection.clone();
        self.views.insert(
            id,
            View {
                id,
                doc,
                selection,
                area: Rect::default(),
                top_line: 0,
                left_col: 0,
                sticky_cols: None,
                jumps: Vec::new(),
                jump_index: 0,
                alternate: None,
            },
        );
        id
    }

    // ----- accessors -------------------------------------------------------

    pub fn view(&self) -> &View {
        &self.views[&self.focus]
    }

    pub fn view_mut(&mut self) -> &mut View {
        self.views
            .get_mut(&self.focus)
            .expect("focused view exists")
    }

    pub fn doc(&self) -> &Document {
        &self.buffers[&self.view().doc].doc
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffers[&self.view().doc]
    }

    pub fn buffer_mut(&mut self) -> &mut Buffer {
        let id = self.view().doc;
        self.buffers.get_mut(&id).expect("view's buffer exists")
    }

    /// The focused view and its buffer, mutably, at the same time.
    pub fn current(&mut self) -> (&mut View, &mut Buffer) {
        let view = self
            .views
            .get_mut(&self.focus)
            .expect("focused view exists");
        let buf = self
            .buffers
            .get_mut(&view.doc)
            .expect("view's buffer exists");
        (view, buf)
    }

    pub fn tab_width(&self) -> usize {
        self.config.editor.tab_width.max(1)
    }

    pub fn set_status(&mut self, severity: Severity, msg: impl Into<String>) {
        self.status = Some((severity, msg.into()));
    }

    /// Sets a status message without hiding a more important one already
    /// showing: both are kept, the more severe first. Used at startup, where
    /// several sources report at once (recovery data, session restore, the
    /// automation socket, config errors).
    pub fn add_status(&mut self, severity: Severity, msg: impl Into<String>) {
        let msg = msg.into();
        self.status = Some(match self.status.take() {
            Some((old_sev, old)) if old_sev >= severity => (old_sev, format!("{old}; {msg}")),
            Some((_, old)) => (severity, format!("{msg}; {old}")),
            None => (severity, msg),
        });
    }

    pub fn info(&mut self, msg: impl Into<String>) {
        self.set_status(Severity::Info, msg);
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.set_status(Severity::Error, msg);
    }

    /// Name shown for a pending char request (`f`, `r`, ...).
    pub fn pending_char_label(&self) -> Option<&str> {
        if self.awaiting_register {
            return Some("register");
        }
        self.pending_char.as_ref().map(|(l, _)| l.as_str())
    }

    /// Asks for the next typed character and passes it to `f`.
    pub fn on_next_char(&mut self, label: &str, f: impl FnOnce(&mut Editor, char) + 'static) {
        self.pending_char = Some((label.to_string(), Box::new(f)));
    }

    /// The keymap node for the currently pending keys, for "which key" hints.
    pub fn pending_node(&self) -> Option<&KeyTrie> {
        if self.pending.is_empty() {
            return None;
        }
        self.keymaps.node(self.mode, &self.pending)
    }

    // ----- editing ---------------------------------------------------------

    /// Applies a transaction to the focused view's document. Other views on
    /// the same document have their selections mapped through the change.
    pub fn apply(&mut self, tx: Transaction) -> Result<(), EditError> {
        let view_id = self.focus;
        self.apply_in_view(view_id, tx)
    }

    pub fn apply_in_view(&mut self, view_id: ViewId, tx: Transaction) -> Result<(), EditError> {
        let doc_id = self.views[&view_id].doc;
        let before = self.views[&view_id].selection.clone();
        let buf = self.buffers.get_mut(&doc_id).expect("buffer exists");
        let old = buf.doc.text().clone();
        buf.doc.apply(&tx, &before)?;
        buf.last_edit = Some(Instant::now());
        let text = buf.doc.text().clone();
        self.on_text_changed(doc_id, &old, std::slice::from_ref(&tx.changes));
        for view in self.views.values_mut().filter(|v| v.doc == doc_id) {
            let mapped = if view.id == view_id {
                tx.selection
                    .clone()
                    .unwrap_or_else(|| view.selection.map(&tx.changes))
            } else {
                view.selection.map(&tx.changes)
            };
            view.selection = mapped.normalize(text.slice(..));
        }
        crate::lsp::document_changed(self, doc_id, std::slice::from_ref(&tx.changes));
        Ok(())
    }

    /// Applies a transaction and reports a failure in the status line.
    pub fn apply_or_report(&mut self, tx: Transaction) -> bool {
        match self.apply(tx) {
            Ok(()) => true,
            Err(e) => {
                self.error(e.to_string());
                false
            }
        }
    }

    pub fn set_selection(&mut self, selection: Selection) {
        let (view, buf) = self.current();
        view.selection = selection.normalize(buf.doc.text().slice(..));
    }

    /// Closes the focused document's undo group.
    pub fn commit(&mut self) {
        let (view, buf) = self.current();
        buf.doc.commit(&view.selection);
    }

    // ----- files, buffers, views -----------------------------------------

    pub fn resolve_path(&self, path: &Path) -> PathBuf {
        let expanded = expand_tilde(path);
        if expanded.is_absolute() {
            expanded
        } else {
            self.cwd.join(expanded)
        }
    }

    pub fn find_buffer_by_path(&self, path: &Path) -> Option<DocumentId> {
        let want = normalize_path(path);
        self.buffers
            .values()
            .find(|b| b.doc.path().map(normalize_path).as_deref() == Some(want.as_path()))
            .map(|b| b.id)
    }

    /// Opens `path` (or switches to it if already open) in the focused view.
    pub fn open(&mut self, path: &Path) -> Result<DocumentId, String> {
        let path = self.resolve_path(path);
        if path.is_dir() {
            return Err(format!("{} is a directory", path.display()));
        }
        let id = match self.find_buffer_by_path(&path) {
            Some(id) => id,
            None => {
                let doc = Document::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                let read_only = doc.read_only();
                let id = self.add_document(doc);
                if let Some(r) = read_only {
                    self.set_status(
                        Severity::Warning,
                        format!("opened read-only: {}", r.describe()),
                    );
                }
                crate::recovery::notify_if_recoverable(self, id);
                id
            }
        };
        self.switch_to(id);
        crate::lsp::reconcile(self);
        crate::navigation::recent::record(self, &path);
        Ok(id)
    }

    /// Shows document `id` in the focused view.
    pub fn switch_to(&mut self, id: DocumentId) {
        if self.view().doc == id {
            return;
        }
        self.push_jump();
        let old = self.view().doc;
        let old_sel = self.view().selection.clone();
        if let Some(b) = self.buffers.get_mut(&old) {
            b.last_selection = old_sel;
        }
        let sel = self.buffers[&id].last_selection.clone();
        let view = self.view_mut();
        view.alternate = Some(old);
        view.doc = id;
        view.selection = sel;
        view.top_line = 0;
        view.left_col = 0;
        view.sticky_cols = None;
        let text = self.doc().text().clone();
        let clamped = self.view().selection.clamp_normal(text.slice(..));
        self.view_mut().selection = clamped;
        self.remove_scratch_if_unused(old);
        self.ensure_cursor_visible();
    }

    /// Drops an untouched, unnamed scratch buffer once nothing shows it, so
    /// opening a file from an empty start does not leave clutter behind.
    pub(crate) fn remove_scratch_if_unused(&mut self, id: DocumentId) {
        let Some(b) = self.buffers.get(&id) else {
            return;
        };
        let untouched = b.doc.path().is_none()
            && !b.doc.is_modified()
            && b.doc.history().is_empty()
            && b.doc.text().len_chars() == 0;
        let shown = self.views.values().any(|v| v.doc == id);
        if untouched && !shown && self.buffers.len() > 1 {
            self.buffers.remove(&id);
            crate::proposals::document_closed(self, id);
            for v in self.views.values_mut() {
                if v.alternate == Some(id) {
                    v.alternate = None;
                }
            }
        }
    }

    pub fn split(&mut self, dir: Direction) {
        let doc = self.view().doc;
        let sel = self.view().selection.clone();
        let top = self.view().top_line;
        let id = self.new_view(doc);
        if let Some(v) = self.views.get_mut(&id) {
            v.selection = sel;
            v.top_line = top;
        }
        self.layout.split(self.focus, id, dir);
        self.focus = id;
        self.relayout();
    }

    /// Closes the focused view. Returns false if it was the last one.
    pub fn close_view(&mut self) -> bool {
        let id = self.focus;
        let order = self.layout.views();
        if !self.layout.remove(id) {
            return false;
        }
        let pos = order.iter().position(|v| *v == id).unwrap_or(0);
        let remaining = self.layout.views();
        let (doc, sel) = {
            let v = &self.views[&id];
            (v.doc, v.selection.clone())
        };
        if let Some(b) = self.buffers.get_mut(&doc) {
            b.last_selection = sel;
        }
        self.views.remove(&id);
        self.focus = remaining[pos
            .min(remaining.len() - 1)
            .saturating_sub(usize::from(pos > 0))];
        self.relayout();
        true
    }

    pub fn only_view(&mut self) {
        let keep = self.focus;
        for id in self.layout.views() {
            if id != keep {
                self.views.remove(&id);
            }
        }
        self.layout.only(keep);
        self.relayout();
    }

    pub fn focus_next(&mut self, delta: isize) {
        let order = self.layout.views();
        let pos = order.iter().position(|v| *v == self.focus).unwrap_or(0) as isize;
        let n = order.len() as isize;
        self.focus = order[((pos + delta).rem_euclid(n)) as usize];
    }

    /// Moves focus to the nearest view in a screen direction.
    pub fn focus_direction(&mut self, dx: i32, dy: i32) {
        let cur = self.view().area;
        let (cx, cy) = (
            cur.x as i32 + cur.width as i32 / 2,
            cur.y as i32 + cur.height as i32 / 2,
        );
        let best = self
            .layout
            .compute(self.area)
            .into_iter()
            .filter(|p| p.view != self.focus)
            .filter(|p| {
                let r = p.rect;
                match (dx, dy) {
                    (1, _) => r.x as i32 >= cur.right() as i32,
                    (-1, _) => (r.right() as i32) <= cur.x as i32,
                    (_, 1) => r.y as i32 >= cur.bottom() as i32,
                    _ => (r.bottom() as i32) <= cur.y as i32,
                }
            })
            .min_by_key(|p| {
                let r = p.rect;
                let (x, y) = (
                    r.x as i32 + r.width as i32 / 2,
                    r.y as i32 + r.height as i32 / 2,
                );
                (x - cx).abs() + (y - cy).abs()
            });
        if let Some(p) = best {
            self.focus = p.view;
        }
    }

    /// Closes a buffer, replacing it in any view that shows it.
    pub fn close_buffer(&mut self, id: DocumentId, force: bool) -> Result<(), String> {
        let b = &self.buffers[&id];
        if b.doc.is_modified() && !force {
            return Err(format!(
                "{} has unsaved changes (add ! to discard them)",
                b.doc.display_name()
            ));
        }
        let path = b.doc.path().map(Path::to_path_buf);
        let replacement = self
            .buffers
            .keys()
            .copied()
            .find(|k| *k != id)
            .unwrap_or_else(|| self.add_document(Document::default()));
        for v in self.views.values_mut().filter(|v| v.doc == id) {
            v.doc = replacement;
            v.selection = Selection::point(0);
            v.top_line = 0;
            v.left_col = 0;
        }
        for v in self.views.values_mut() {
            if v.alternate == Some(id) {
                v.alternate = None;
            }
            v.jumps.retain(|(d, _)| *d != id);
            v.jump_index = v.jump_index.min(v.jumps.len());
        }
        self.marks.retain(|_, (d, _)| *d != id);
        self.buffers.remove(&id);
        crate::proposals::document_closed(self, id);
        if force && let (Some(dir), Some(path)) = (&self.recovery_dir, path) {
            crate::recovery::remove(dir, &path, &self.session_id);
        }
        let sel = self.buffers[&replacement].last_selection.clone();
        let text = self.buffers[&replacement].doc.text().clone();
        for v in self.views.values_mut().filter(|v| v.doc == replacement) {
            if v.selection == Selection::point(0) {
                v.selection = sel.clamp_normal(text.slice(..));
            }
        }
        Ok(())
    }

    pub fn modified_buffers(&self) -> Vec<String> {
        self.buffers
            .values()
            .filter(|b| b.doc.is_modified())
            .map(|b| b.doc.display_name())
            .collect()
    }

    // ----- jumps ----------------------------------------------------------

    pub fn push_jump(&mut self) {
        let doc = self.view().doc;
        let sel = self.view().selection.clone();
        let view = self.view_mut();
        view.jumps.truncate(view.jump_index);
        if view
            .jumps
            .last()
            .is_some_and(|(d, s)| *d == doc && *s == sel)
        {
            return;
        }
        view.jumps.push((doc, sel));
        if view.jumps.len() > 100 {
            view.jumps.remove(0);
        }
        view.jump_index = view.jumps.len();
    }

    pub fn jump(&mut self, backward: bool) {
        let (idx, len) = (self.view().jump_index, self.view().jumps.len());
        if backward {
            if idx == 0 {
                return;
            }
            if idx == len {
                // Remember where we are so jumping forward returns here.
                self.push_jump();
                self.view_mut().jump_index = len;
            }
            self.view_mut().jump_index -= 1;
        } else {
            if idx + 1 >= self.view().jumps.len() {
                return;
            }
            self.view_mut().jump_index += 1;
        }
        let (doc, sel) = self.view().jumps[self.view().jump_index].clone();
        if !self.buffers.contains_key(&doc) {
            return;
        }
        let idx = self.view().jump_index;
        let jumps = self.view().jumps.clone();
        if self.view().doc != doc {
            let old = self.view().doc;
            let old_sel = self.view().selection.clone();
            if let Some(b) = self.buffers.get_mut(&old) {
                b.last_selection = old_sel;
            }
            let v = self.view_mut();
            v.alternate = Some(old);
            v.doc = doc;
        }
        let text = self.doc().text().clone();
        let v = self.view_mut();
        v.selection = sel.clamp_normal(text.slice(..));
        v.jumps = jumps;
        v.jump_index = idx;
    }

    // ----- geometry -------------------------------------------------------

    pub fn resize(&mut self, area: Rect) {
        self.area = area;
        self.relayout();
    }

    pub fn relayout(&mut self) {
        for p in self.layout.compute(self.area) {
            if let Some(v) = self.views.get_mut(&p.view) {
                v.area = p.rect;
            }
        }
        let ids: Vec<ViewId> = self.views.keys().copied().collect();
        for id in ids {
            self.ensure_visible_in(id);
        }
    }

    /// Width of the gutter (line numbers plus a sign column) for a view.
    pub fn gutter_width(&self, view: &View) -> u16 {
        if self.presentation == Presentation::Focus {
            return 0;
        }
        // A blame column (`space h B`) sits left of the sign column.
        let blame = crate::git_hunks::blame_width(self, view.doc);
        if self.config.editor.line_numbers == LineNumbers::Off {
            return blame + 1;
        }
        let lines = crate::motion::last_line(self.buffers[&view.doc].doc.text().slice(..)) + 1;
        let digits = lines.to_string().len().max(3) as u16;
        blame + 1 + digits + 1
    }

    pub fn text_width(&self, view: &View) -> usize {
        view.area.width.saturating_sub(self.gutter_width(view)) as usize
    }

    pub fn ensure_cursor_visible(&mut self) {
        self.ensure_visible_in(self.focus);
    }

    pub(crate) fn ensure_visible_in(&mut self, id: ViewId) {
        let Some(view) = self.views.get(&id) else {
            return;
        };
        let text = self.buffers[&view.doc].doc.text().slice(..);
        let head = view.selection.primary().head.min(text.len_chars());
        let line = text.char_to_line(head);
        let col = crate::motion::display_col(text, head, self.tab_width());
        let height = view.text_height().max(1);
        let width = self.text_width(view).max(1);
        let so = self
            .config
            .editor
            .scrolloff
            .min(height.saturating_sub(1) / 2);
        let mut top = view.top_line;
        if line < top + so {
            top = line.saturating_sub(so);
        } else if line + so >= top + height {
            top = line + so + 1 - height;
        }
        let max_top = crate::motion::last_line(text);
        top = top.min(max_top);
        let mut left = view.left_col;
        if col < left {
            left = col;
        } else if col >= left + width {
            left = col + 1 - width;
        }
        let v = self.views.get_mut(&id).expect("view exists");
        v.top_line = top;
        v.left_col = left;
    }

    /// Scrolls the view by `delta` lines and pulls the cursor along if it
    /// would leave the screen.
    pub fn scroll(&mut self, delta: isize) {
        let tab = self.tab_width();
        let (view, buf) = self.current();
        let text = buf.doc.text().slice(..);
        let last = crate::motion::last_line(text);
        let height = view.text_height().max(1);
        view.top_line = (view.top_line as isize + delta).clamp(0, last as isize) as usize;
        let head = view.selection.primary().head;
        let line = text.char_to_line(head.min(text.len_chars()));
        let target = line.clamp(view.top_line, (view.top_line + height - 1).min(last));
        if target != line {
            let col = crate::motion::display_col(text, head, tab);
            let p = maestro_core::pos::line_col_to_char(
                text,
                maestro_core::pos::LineCol { line: target, col },
                tab,
            );
            view.selection = Selection::point(p);
        }
    }

    // ----- key handling ---------------------------------------------------

    /// Handles one key press.
    pub fn handle_key(&mut self, key: KeyEvent) {
        if let Some((_, keys)) = &mut self.recording {
            keys.push(key);
        }
        let version_before = self.doc().version();
        let doc_before = self.view().doc;
        let mode_before = self.mode;
        let was_idle = self.pending.is_empty()
            && self.count.is_none()
            && self.selected_register.is_none()
            && self.pending_char.is_none()
            && !self.awaiting_register
            && self.prompt.is_none()
            && self.picker.is_none();
        if !self.replaying {
            if was_idle && mode_before != Mode::Insert {
                self.change_keys.clear();
            }
            if self.mode != Mode::Insert || self.in_insert_change {
                self.change_keys.push(key);
            }
        }

        self.status_clear_on_key(key);
        crate::lsp::on_key(self);
        crate::git_hunks::on_key(self);
        self.dispatch(key);

        // Mode-transition bookkeeping.
        if self.mode != Mode::Insert {
            self.commit();
            let text = self.doc().text().clone();
            let sel = self.view().selection.clamp_normal(text.slice(..));
            self.view_mut().selection = sel;
        }
        self.track_change(version_before, doc_before, mode_before);
        crate::lsp::reconcile(self);
        crate::lsp::completion::after_key(self, key, mode_before, doc_before, version_before);
        crate::lsp::signature::after_key(self, key, mode_before, doc_before, version_before);
        self.ensure_cursor_visible();
    }

    /// True while `.` or a macro replays keys. Completion stays off then:
    /// a replay must not depend on a server's answer.
    pub(crate) fn is_replaying(&self) -> bool {
        self.replaying || self.macro_depth > 0
    }

    /// Removes `key`, just pushed by [`Editor::handle_key`], from the keys
    /// recorded for `.` and for a macro. The completion popup calls this for
    /// the keys it uses (moving, accepting, closing), which mean nothing
    /// without the popup.
    pub(crate) fn forget_recorded_key(&mut self, key: KeyEvent) {
        if let Some((_, keys)) = &mut self.recording
            && keys.last() == Some(&key)
        {
            keys.pop();
        }
        if !self.replaying && self.in_insert_change && self.change_keys.last() == Some(&key) {
            self.change_keys.pop();
        }
    }

    /// Records an accepted completion for `.` and macros. `Some(keys)`:
    /// keys that, typed, make the same edit; they stand in for the
    /// completion. `None`: typing cannot reproduce it, so this insert
    /// session is not repeatable with `.` and a macro being recorded stops
    /// with a warning (replaying it would make a different edit).
    pub(crate) fn record_completion(&mut self, keys: Option<Vec<KeyEvent>>) {
        match keys {
            Some(keys) => {
                if let Some((_, rec)) = &mut self.recording {
                    rec.extend_from_slice(&keys);
                }
                if !self.replaying && self.in_insert_change {
                    self.change_keys.extend_from_slice(&keys);
                }
            }
            None => {
                if !self.replaying {
                    self.in_insert_change = false;
                    self.change_keys.clear();
                    self.last_change.clear();
                }
                if let Some((reg, _)) = self.recording.take() {
                    self.set_status(
                        Severity::Warning,
                        format!(
                            "recording into register {reg} stopped: this completion \
                             cannot be replayed as typed keys"
                        ),
                    );
                }
            }
        }
    }

    fn status_clear_on_key(&mut self, _key: KeyEvent) {
        // Keep errors visible until the next completed action; clear info.
        if self.prompt.is_none() && self.pending.is_empty() {
            self.status = None;
        }
    }

    fn track_change(
        &mut self,
        version_before: DocumentVersion,
        doc_before: DocumentId,
        mode_before: Mode,
    ) {
        if self.replaying {
            return;
        }
        let idle = self.pending.is_empty()
            && self.count.is_none()
            && self.pending_char.is_none()
            && !self.awaiting_register
            && self.selected_register.is_none()
            && self.prompt.is_none()
            && self.picker.is_none();
        if self.mode == Mode::Insert && mode_before != Mode::Insert {
            self.in_insert_change = true;
            return;
        }
        if self.mode != Mode::Insert && mode_before == Mode::Insert && self.in_insert_change {
            self.in_insert_change = false;
            self.last_change = std::mem::take(&mut self.change_keys);
            return;
        }
        let unrepeatable = std::mem::take(&mut self.unrepeatable);
        if idle
            && self.mode != Mode::Insert
            && self.view().doc == doc_before
            && self.doc().version() != version_before
        {
            if unrepeatable {
                // `.` keeps repeating the change before this one.
                self.change_keys.clear();
            } else {
                self.last_change = std::mem::take(&mut self.change_keys);
            }
        }
    }

    /// Marks the current key's edit as one `.` must not repeat: replaying
    /// the keys that led to it (a picker query and Enter, say) would not
    /// make the same edit, because the picker they went to depends on a
    /// language server's answer. A macro being recorded stops with a
    /// warning, for the same reason.
    pub(crate) fn not_repeatable(&mut self, what: &str) {
        if !self.replaying {
            self.unrepeatable = true;
        }
        if let Some((reg, _)) = self.recording.take() {
            self.set_status(
                Severity::Warning,
                format!("recording into register {reg} stopped: {what} cannot be replayed"),
            );
        }
    }

    /// Replays the last change (`.`).
    pub fn repeat_last_change(&mut self) {
        let keys = self.last_change.clone();
        if keys.is_empty() {
            return;
        }
        let was_replaying = std::mem::replace(&mut self.replaying, true);
        for k in &keys {
            self.handle_key(*k);
        }
        self.replaying = was_replaying;
        // The `.` that triggered this must not become the last change.
        self.change_keys = keys;
    }

    /// Feeds keys as if typed, for macros and tests.
    pub fn feed_keys(&mut self, keys: &[KeyEvent]) {
        for k in keys {
            self.handle_key(*k);
        }
    }

    fn dispatch(&mut self, key: KeyEvent) {
        if let Some(picker) = &mut self.picker {
            let outcome = picker.handle_key(key);
            crate::picker::resolve(self, outcome);
            return;
        }
        if self.prompt.is_some() {
            crate::prompt::handle_key(self, key);
            return;
        }
        if self.awaiting_register {
            self.awaiting_register = false;
            match key.typed_char() {
                Some(c) => self.selected_register = Some(c),
                None => self.count = None,
            }
            return;
        }
        if let Some((_, f)) = self.pending_char.take() {
            if let Some(c) = key.typed_char() {
                f(self, c);
            } else if key.code == KeyCode::Enter {
                f(self, '\n');
            } else if key.code == KeyCode::Tab {
                f(self, '\t');
            }
            self.count = None;
            self.selected_register = None;
            return;
        }
        // The focused sidebar takes its own keys; anything else (a `space`
        // sequence) goes to the normal keymap, never to what is behind it.
        let sidebar = self.workspaces.sidebar.has_focus();
        if sidebar && self.pending.is_empty() && crate::workspaces::sidebar::handle_key(self, key) {
            return;
        }
        if !sidebar && self.forge.screen_open() {
            crate::forge::screen::handle_key(self, key);
            return;
        }
        if !sidebar && self.git.screen_open() {
            crate::git_screen::handle_key(self, key);
            return;
        }
        if !sidebar && self.proposals.review_open() {
            crate::proposals::review::handle_key(self, key);
            return;
        }
        if !sidebar && self.agents.panel_open() {
            crate::agents::panel::handle_key(self, key);
            return;
        }
        if !sidebar && self.mode == Mode::Insert {
            // An open completion popup takes its keys first.
            if self.pending.is_empty() && crate::lsp::completion::handle_key(self, key) {
                return;
            }
            self.dispatch_insert(key);
            return;
        }
        // Count prefix.
        if self.pending.is_empty()
            && let Some(c) = key.typed_char()
            && let Some(d) = c.to_digit(10)
            && (d != 0 || self.count.is_some())
        {
            self.count = Some(
                self.count
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(d as usize)
                    .min(1_000_000),
            );
            return;
        }
        if self.pending.is_empty() && key == KeyEvent::char('"') {
            // Keeps any count typed before the register prefix.
            self.awaiting_register = true;
            return;
        }
        self.pending.push(key);
        match self.keymaps.lookup(self.mode, &self.pending) {
            Lookup::Command(name) => {
                self.pending.clear();
                if sidebar && name != "toggle_sidebar" {
                    // A command typed from the sidebar acts on the editor.
                    self.workspaces.sidebar.focused = false;
                }
                self.run_command(&name);
                if self.pending_char.is_none() {
                    self.count = None;
                    self.selected_register = None;
                }
            }
            Lookup::Pending => {}
            Lookup::None => {
                let keys: Vec<String> = self.pending.iter().map(ToString::to_string).collect();
                self.pending.clear();
                self.count = None;
                self.selected_register = None;
                if key.code != KeyCode::Esc {
                    self.set_status(
                        Severity::Warning,
                        format!("no binding for {}", keys.join(" ")),
                    );
                }
            }
        }
    }

    fn dispatch_insert(&mut self, key: KeyEvent) {
        self.pending.push(key);
        match self.keymaps.lookup(Mode::Insert, &self.pending) {
            Lookup::Command(name) => {
                self.pending.clear();
                self.run_command(&name);
            }
            Lookup::Pending => {}
            Lookup::None => {
                self.pending.clear();
                if let Some(c) = key.typed_char() {
                    crate::commands::insert_char(self, c);
                }
            }
        }
    }

    /// Runs a command from the registry by name. Unknown names report an
    /// error rather than panicking, since names can come from user config.
    pub fn run_command(&mut self, name: &str) {
        // Vertical motions remember their column; anything else resets it.
        if !matches!(
            name,
            "move_line_up"
                | "move_line_down"
                | "half_page_up"
                | "half_page_down"
                | "page_up"
                | "page_down"
        ) {
            self.view_mut().sticky_cols = None;
        }
        match crate::commands::lookup(name) {
            Some(cmd) => (cmd.fun)(self),
            None => match crate::typed::lookup(name) {
                Some(_) => crate::typed::execute(self, name),
                None => self.error(format!("unknown command: {name}")),
            },
        }
    }

    pub fn count_or(&self, default: usize) -> usize {
        self.count.unwrap_or(default).max(1)
    }

    /// Selection mode helper: motions extend in Select mode.
    pub fn extending(&self) -> bool {
        self.mode == Mode::Select
    }

    /// Handles a mouse event at screen cell (x, y).
    pub fn handle_mouse(&mut self, kind: MouseKind, x: u16, y: u16) {
        // The panel takes the mouse only when it is the screen on top:
        // the inbox, the Git screen and the proposal review draw over it.
        if self.agents.panel_open()
            && self.picker.is_none()
            && self.prompt.is_none()
            && !self.forge.screen_open()
            && !self.git.screen_open()
            && !self.proposals.review_open()
        {
            crate::agents::panel::handle_mouse(self, kind, x, y);
            return;
        }
        if self.picker.is_some()
            || self.prompt.is_some()
            || self.agents.panel_open()
            || self.proposals.review_open()
            || self.forge.screen_open()
            || self.git.screen_open()
        {
            return;
        }
        let Some(view_id) = self
            .layout
            .compute(self.area)
            .into_iter()
            .find(|p| p.rect.contains(x, y))
            .map(|p| p.view)
        else {
            return;
        };
        match kind {
            MouseKind::ScrollUp | MouseKind::ScrollDown => {
                let prev = self.focus;
                self.focus = view_id;
                self.scroll(if kind == MouseKind::ScrollUp { -3 } else { 3 });
                self.focus = prev;
            }
            MouseKind::Down | MouseKind::Drag => {
                if kind == MouseKind::Down {
                    self.focus = view_id;
                    self.workspaces.sidebar.focused = false;
                } else if self.focus != view_id {
                    return;
                }
                let view = &self.views[&view_id];
                let gutter = self.gutter_width(view);
                if y >= view.area.y + view.text_height() as u16 || x < view.area.x + gutter {
                    return;
                }
                let text = self.buffers[&view.doc].doc.text().clone();
                let s = text.slice(..);
                let line = view.top_line + (y - view.area.y) as usize;
                if line > crate::motion::last_line(s) {
                    return;
                }
                let col = view.left_col + (x - view.area.x - gutter) as usize;
                let pos = maestro_core::pos::line_col_to_char(
                    s,
                    maestro_core::pos::LineCol { line, col },
                    self.tab_width(),
                );
                if self.mode == Mode::Insert && kind == MouseKind::Down {
                    self.commit();
                }
                let sel = if kind == MouseKind::Drag {
                    let anchor = self.view().selection.primary().anchor;
                    Selection::single(anchor, pos)
                } else {
                    Selection::point(pos)
                };
                self.set_selection(sel);
                if self.mode != Mode::Insert {
                    let sel = self.view().selection.clamp_normal(s);
                    self.view_mut().selection = sel;
                }
            }
        }
    }

    /// Inserts pasted text (bracketed paste). In insert mode it goes in at
    /// each cursor; otherwise it replaces nothing and lands before each
    /// selection, selected. One paste is one undo step.
    pub fn paste_text(&mut self, text: &str) {
        // Overlays own the input: a paste while one is open goes into it,
        // never into the document underneath.
        if let Some(p) = &mut self.picker {
            p.push_query(text);
            return;
        }
        if self.prompt.is_some() {
            crate::prompt::insert_text(self, text);
            return;
        }
        if self.workspaces.sidebar.has_focus() {
            // Never into the document behind the focused sidebar.
            self.info("paste ignored: the sidebar has the keys (esc returns to the editor)");
            return;
        }
        if self.forge.screen_open() {
            // The inbox has no text input; messages are written in buffers.
            return;
        }
        if self.proposals.review_open() {
            crate::proposals::review::paste(self, text);
            return;
        }
        if self.agents.panel_open() {
            crate::agents::panel::paste(self, text);
            return;
        }
        if self.git.screen_open() {
            // The status screen has no text input; a paste does nothing.
            return;
        }
        if self.pending_char.is_some() || self.awaiting_register || !self.pending.is_empty() {
            // A paste in the middle of a key sequence cancels it.
            self.pending_char = None;
            self.awaiting_register = false;
            self.pending.clear();
            self.count = None;
            return;
        }
        let text = if self.doc().line_ending() == maestro_core::LineEnding::Crlf {
            text.replace("\r\n", "\n").replace('\n', "\r\n")
        } else {
            text.replace("\r\n", "\n")
        };
        let doc_text = self.doc().text().clone();
        let insert = self.mode == Mode::Insert;
        let changes = self
            .view()
            .selection
            .iter()
            .map(|r| {
                let at = if insert { r.head } else { r.from() };
                maestro_core::Change::insert(at, text.clone())
            })
            .collect::<Vec<_>>();
        let cs = maestro_core::ChangeSet::from_changes_clipped(doc_text.len_chars(), changes);
        let n = text.chars().count();
        let primary = self.view().selection.primary_index();
        let ranges: Vec<Range> = self
            .view()
            .selection
            .iter()
            .map(|r| {
                let at = if insert { r.head } else { r.from() };
                let start = cs.map_pos(at, maestro_core::Assoc::Before);
                if insert || n == 0 {
                    Range::point(start + n)
                } else {
                    Range::new(start + n - 1, start)
                }
            })
            .collect();
        let tx = Transaction::new(cs).with_selection(Selection::new(ranges, primary));
        self.apply_or_report(tx);
        if !insert {
            self.commit();
        }
        self.ensure_cursor_visible();
    }

    /// Runs `work` on a new thread. The thread reports back through the
    /// [`JobSender`] it is given; results are applied in
    /// [`Editor::poll_background`] on the main thread.
    pub fn spawn_background(
        &self,
        name: &str,
        work: impl FnOnce(JobSender) + Send + 'static,
    ) -> std::io::Result<()> {
        self.active_jobs
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let sender = JobSender {
            tx: self.jobs_tx.clone(),
            wake: self.wake.clone(),
            _active: std::sync::Arc::new(ActiveGuard(Some(self.active_jobs.clone()))),
        };
        std::thread::Builder::new()
            .name(format!("maestro-{name}"))
            .spawn(move || work(sender))
            .map(|_| ())
    }

    /// A sender for long-lived producers (servers, sockets) that should not
    /// count as running work in [`Editor::has_background_work`]. Prefer
    /// [`Editor::spawn_background`].
    pub fn idle_job_sender(&self) -> JobSender {
        JobSender {
            tx: self.jobs_tx.clone(),
            wake: self.wake.clone(),
            _active: std::sync::Arc::new(ActiveGuard(None)),
        }
    }

    /// Installs the front end's wake-up callback. Only the first call takes
    /// effect; returns false for later calls.
    pub fn set_waker(&self, f: impl Fn() + Send + Sync + 'static) -> bool {
        self.wake.0.set(Box::new(f)).is_ok()
    }

    /// A handle that wakes the front end, for producers that change shared
    /// state without sending a job (terminal output, for example).
    pub fn wake_handle(&self) -> WakeHandle {
        self.wake.clone()
    }

    /// True while background work started with `spawn_background` runs.
    pub fn has_background_work(&self) -> bool {
        self.active_jobs.load(std::sync::atomic::Ordering::SeqCst) > 0
    }

    /// Applies results sent by background threads. Returns true if any ran.
    /// Also starts Git sign updates for buffers that changed.
    pub fn poll_background(&mut self) -> bool {
        crate::git::refresh(self);
        let mut ran = false;
        while let Ok(job) = self.jobs_rx.try_recv() {
            job(self);
            ran = true;
        }
        if ran {
            self.ensure_cursor_visible();
        }
        // Terminal output only wakes the loop; this notices it.
        let changed = crate::agents::poll(self) || ran;
        crate::workspaces::check_attention(self) || changed
    }

    /// Bookkeeping every text change needs, whatever caused it: the buffer's
    /// cached selection, an open prompt's saved selection, marks and jump
    /// entries follow the edit, and the recovery idle timer restarts. View
    /// selections are mapped by the caller (it knows which view is acting).
    /// Called from [`Editor::on_text_changed`], which every path calls.
    pub fn track_text_change(&mut self, id: DocumentId, changes: &[maestro_core::ChangeSet]) {
        let focused_doc = self.views.get(&self.focus).map(|v| v.doc);
        let Some(buf) = self.buffers.get_mut(&id) else {
            return;
        };
        buf.last_edit = Some(Instant::now());
        let text = buf.doc.text().clone();
        let s = text.slice(..);
        let map = |sel: &Selection| {
            changes
                .iter()
                .fold(sel.clone(), |sel, cs| sel.map(cs))
                .normalize(s)
        };
        buf.last_selection = map(&buf.last_selection).clamp_normal(s);
        if focused_doc == Some(id)
            && let Some(p) = self.prompt.as_mut()
        {
            p.original = map(&p.original);
        }
        for (doc, pos) in self.marks.values_mut() {
            if *doc == id {
                *pos = changes
                    .iter()
                    .fold(*pos, |p, cs| cs.map_pos(p, maestro_core::Assoc::Before))
                    .min(text.len_chars());
            }
        }
        for view in self.views.values_mut() {
            for (doc, sel) in &mut view.jumps {
                if *doc == id {
                    *sel = map(sel);
                }
            }
        }
    }

    /// Runs a `:` command line, as if typed after `:`.
    pub fn execute_line(&mut self, line: &str) {
        crate::typed::execute_line(self, line);
        self.commit();
        // Same invariant as after a key: no normal-mode cursor past the text.
        if self.mode != Mode::Insert {
            let text = self.doc().text().clone();
            let sel = self.view().selection.clamp_normal(text.slice(..));
            self.view_mut().selection = sel;
        }
        self.ensure_cursor_visible();
    }

    /// Called periodically by the front end (roughly once a second) for
    /// background duties: recovery files, external-change detection and
    /// noticing Git index or branch changes.
    pub fn tick(&mut self, now: Instant) {
        crate::recovery::tick(self, now);
        self.check_disk_changes();
        crate::git::tick(self);
        crate::lsp::reconcile(self);
        crate::lsp::stop_unused(self, now);
        crate::agents::tick(self, now);
        crate::workspaces::tick(self, now);
        crate::forge::tick(self, now);
    }

    fn check_disk_changes(&mut self) {
        let ids: Vec<DocumentId> = self.buffers.keys().copied().collect();
        for id in ids {
            let b = &self.buffers[&id];
            if b.disk_change_reported || !b.doc.changed_on_disk() {
                continue;
            }
            let name = b.doc.display_name();
            let path_exists = b.doc.path().is_some_and(Path::exists);
            if !b.doc.is_modified() && path_exists {
                let sel = self
                    .views
                    .values()
                    .find(|v| v.doc == id)
                    .map(|v| v.selection.clone())
                    .unwrap_or_else(|| Selection::point(0));
                let buf = self.buffers.get_mut(&id).expect("exists");
                let old = buf.doc.text().clone();
                match buf.doc.reload(&sel) {
                    Ok(changes) => {
                        let text = buf.doc.text().clone();
                        self.on_text_changed(id, &old, std::slice::from_ref(&changes));
                        for v in self.views.values_mut().filter(|v| v.doc == id) {
                            v.selection = v.selection.map(&changes).clamp_normal(text.slice(..));
                        }
                        crate::lsp::document_changed(self, id, &[changes]);
                        self.info(format!("reloaded {name}: it changed on disk"));
                    }
                    Err(e) => self.error(format!("could not reload {name}: {e}")),
                }
            } else {
                let buf = self.buffers.get_mut(&id).expect("exists");
                buf.disk_change_reported = true;
                let what = if path_exists {
                    "changed"
                } else {
                    "was deleted"
                };
                self.set_status(
                    Severity::Warning,
                    format!("{name} {what} on disk; :reload discards your edits, :w! keeps them"),
                );
            }
        }
    }

    /// Time between recovery writes for a modified buffer.
    pub fn recovery_interval(&self) -> Option<Duration> {
        match self.config.editor.recovery_interval_secs {
            0 => None,
            s => Some(Duration::from_secs(s)),
        }
    }

    /// The selection of the focused view, collapsed to points if the given
    /// ranges should be treated as insertion points.
    pub fn primary(&self) -> Range {
        self.view().selection.primary()
    }
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
}

/// Lexically normalizes a path (`a/./b/../c` to `a/c`) and resolves it
/// through the filesystem when it exists, so two spellings of one file map
/// to one buffer.
pub fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(p) = std::fs::canonicalize(path) {
        return p;
    }
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}
