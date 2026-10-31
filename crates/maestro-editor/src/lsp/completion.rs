//! Completion in insert mode: when to ask, the popup's state, filtering,
//! and applying the accepted item.
//!
//! A request goes out
//!
//! * at once when the typed character is one of the server's
//!   `triggerCharacters`;
//! * [`DEBOUNCE`] after an identifier character, if no other key came in
//!   between;
//! * at once on `C-x` (the `completion` command).
//!
//! The reply opens a popup filtered by the identifier typed before the
//! cursor. Typing narrows it; when the server marked the list incomplete,
//! typing also asks again (debounced). `C-n`/`C-p` and Down/Up move, Tab or
//! Enter accepts, Escape closes the popup and stays in insert mode (as
//! Escape closes a picker or prompt and does nothing else). Any other key
//! that moves the cursor without editing closes it.
//!
//! Positions in the reply are converted to char indices against the text
//! the request was made for (the reply guard makes sure that is still the
//! current text), then mapped through every later edit, so typing more
//! before accepting extends the replaced range the way it should.

use std::time::Duration;

use maestro_core::text::prev_grapheme_boundary;
use maestro_core::{
    Assoc, Change, ChangeSet, DocumentVersion, Range, Rope, Selection, Transaction,
};
use maestro_lsp::completion::{self as proto, ItemEdit};
use maestro_lsp::convert;
use maestro_lsp::{Reply, RequestId, RpcError};
/// For building a result to pass to [`show`].
pub use maestro_lsp::{Value, json};

use super::{REQUEST_TIMEOUT, ServerId, reconcile};
use crate::editor::{DocumentId, Editor, Mode, Severity, ViewId};
use crate::key::{KeyCode, KeyEvent, Modifiers};

/// Quiet time after an identifier character before asking the server.
/// A fast typist's keys come 80 to 120 ms apart, so a burst sends one
/// request at its end; 150 ms is short enough that the popup still appears
/// as soon as the typing stops.
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// Rows the popup shows at once.
pub const MAX_VISIBLE: usize = 10;

/// Why a request was sent. Maps to the protocol's `CompletionTriggerKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// `C-x`.
    Manual,
    /// An identifier character, after the debounce.
    Typing,
    /// One of the server's trigger characters.
    Character(char),
    /// The open list is incomplete and the prefix changed.
    Incomplete,
}

impl Trigger {
    fn context(self) -> maestro_lsp::Value {
        match self {
            Trigger::Manual | Trigger::Typing => json!({"triggerKind": 1}),
            Trigger::Character(c) => json!({"triggerKind": 2, "triggerCharacter": c.to_string()}),
            Trigger::Incomplete => json!({"triggerKind": 3}),
        }
    }
}

/// One item, with its ranges in char coordinates of the current text.
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub label: String,
    /// The kind as a word (`function`, `module`), or empty.
    pub kind: &'static str,
    pub detail: String,
    filter: String,
    sort: String,
    preselect: bool,
    /// What the main edit replaces. `None`: the identifier before the
    /// cursor.
    range: Option<(usize, usize)>,
    text: String,
    snippet: bool,
    additional: Vec<(usize, usize, String)>,
    /// An edit touched one of the additional edits' ranges, so where they
    /// go is no longer known. The item is no longer offered.
    stale: bool,
}

/// An item that matches the typed prefix, with the label chars to
/// highlight (empty when the item filters on text other than its label).
#[derive(Debug, Clone)]
pub struct MenuMatch {
    pub index: usize,
    pub positions: Vec<usize>,
}

/// The open popup.
#[derive(Debug, Clone)]
pub struct Menu {
    pub doc: DocumentId,
    pub view: ViewId,
    /// Char index where the identifier being completed starts.
    pub start: usize,
    /// The primary cursor, document version and cursors the popup is in
    /// step with. If any differs, the popup is stale and closes.
    pub head: usize,
    pub version: DocumentVersion,
    selection: Selection,
    /// The version the stored ranges were last mapped to.
    mapped: DocumentVersion,
    pub items: Vec<MenuItem>,
    pub matches: Vec<MenuMatch>,
    pub selected: usize,
    /// The server said typing more needs a new request.
    pub incomplete: bool,
}

impl Menu {
    pub fn selected_item(&self) -> Option<&MenuItem> {
        self.matches
            .get(self.selected)
            .map(|m| &self.items[m.index])
    }

    /// Filters and ranks the items by the text from `start` to `head`:
    /// fuzzy score first, then the server's `sortText`, then the label.
    fn refilter(&mut self, text: &Rope) {
        let query = text.slice(self.start..self.head).to_string();
        let mut scored: Vec<(i64, MenuMatch)> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| !item.stale)
            .filter_map(|(index, item)| {
                let (score, positions) = crate::picker::fuzzy_match(&query, &item.filter)?;
                let positions = if item.filter == item.label {
                    positions
                } else {
                    Vec::new()
                };
                Some((score, MenuMatch { index, positions }))
            })
            .collect();
        scored.sort_by(|(sa, a), (sb, b)| {
            let (ia, ib) = (&self.items[a.index], &self.items[b.index]);
            sb.cmp(sa)
                .then_with(|| ib.preselect.cmp(&ia.preselect))
                .then_with(|| ia.sort.cmp(&ib.sort))
                .then_with(|| ia.label.cmp(&ib.label))
        });
        self.matches = scored.into_iter().map(|(_, m)| m).collect();
        self.selected = 0;
    }
}

struct Pending {
    token: u64,
    server: ServerId,
    id: RequestId,
    doc: DocumentId,
    view: ViewId,
    version: DocumentVersion,
    selection: Selection,
    trigger: Trigger,
}

/// Completion state, part of [`super::Lsp`].
#[derive(Default)]
pub struct Completion {
    pub menu: Option<Menu>,
    pending: Option<Pending>,
    /// Bumped on every key. A debounce timer only fires if it is unchanged.
    debounce: u64,
    next_token: u64,
    /// Why the last reply was not shown, for tests and diagnosis.
    pub last_dropped: Option<String>,
}

impl Completion {
    /// True while a completion request is out.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Start of the identifier that ends at `head`.
fn word_start(text: &Rope, head: usize) -> usize {
    let mut i = head;
    while i > 0 && is_word(text.char(i - 1)) {
        i -= 1;
    }
    i
}

/// The popup, if it is in step with the focused view and has matches.
pub fn visible(ed: &Editor) -> Option<&Menu> {
    let menu = ed.lsp.completion.menu.as_ref()?;
    (in_step(ed, menu) && !menu.matches.is_empty() && usable(ed)).then_some(menu)
}

fn in_step(ed: &Editor, menu: &Menu) -> bool {
    let sel = &ed.view().selection;
    ed.mode == Mode::Insert
        && ed.prompt.is_none()
        && ed.picker.is_none()
        && ed.focus == menu.view
        && ed.view().doc == menu.doc
        && ed.doc().version() == menu.version
        && *sel == menu.selection
}

/// Smallest view area the popup is drawn in. Below it the popup is neither
/// drawn nor takes keys, so Tab and Enter never accept an item the user
/// cannot see. The front end draws it whenever this holds.
pub const MIN_AREA: (u16, u16) = (8, 8);

fn usable(ed: &Editor) -> bool {
    ed.area.width >= MIN_AREA.0 && ed.area.height >= MIN_AREA.1
}

/// Closes the popup and cancels any request or timer behind it.
pub fn close(ed: &mut Editor) {
    let c = &mut ed.lsp.completion;
    c.menu = None;
    c.debounce += 1;
    if let Some(p) = c.pending.take()
        && let Some(s) = ed.lsp.servers.get(&p.server)
    {
        s.client.cancel(p.id);
    }
}

/// Forgets requests to a server that stopped.
pub(super) fn server_stopped(ed: &mut Editor, server: ServerId) {
    let c = &mut ed.lsp.completion;
    if c.pending.as_ref().is_some_and(|p| p.server == server) {
        c.pending = None;
    }
    c.menu = None;
}

/// Hook at the start of every key: a pending debounce is superseded.
pub(super) fn on_key(ed: &mut Editor) {
    ed.lsp.completion.debounce += 1;
}

/// Maps the popup's positions through edits to `doc`.
pub(super) fn document_changed(ed: &mut Editor, doc: DocumentId, changes: &[ChangeSet]) {
    let Some(menu) = ed.lsp.completion.menu.as_mut() else {
        return;
    };
    if menu.doc != doc {
        return;
    }
    let Some(version) = ed.buffers.get(&doc).map(|b| b.doc.version()) else {
        ed.lsp.completion.menu = None;
        return;
    };
    let changes: Vec<&ChangeSet> = changes.iter().filter(|c| !c.is_empty()).collect();
    if changes.is_empty() {
        // The version moved without a change set to map through.
        if version != menu.mapped {
            ed.lsp.completion.menu = None;
        }
        return;
    }
    for cs in changes {
        // Deleting the character before the identifier (a trigger `.`,
        // say) or anything across its start ends this completion.
        let cuts_start = cs
            .changes()
            .iter()
            .any(|c| c.from < menu.start && c.to >= menu.start && c.to > c.from);
        if cuts_start {
            ed.lsp.completion.menu = None;
            return;
        }
        menu.start = cs.map_pos(menu.start, Assoc::Before);
        let edits = cs.changes();
        for item in &mut menu.items {
            let touched = item
                .additional
                .iter()
                .any(|&(a, b, _)| edits.iter().any(|c| c.from <= b && c.to >= a));
            if touched {
                item.stale = true;
            }
            if let Some((a, b)) = &mut item.range {
                *a = cs.map_pos(*a, Assoc::Before);
                *b = cs.map_pos(*b, Assoc::After).max(*a);
            }
            // Extra edits never swallow text typed at their edges.
            for (a, b, _) in &mut item.additional {
                *a = cs.map_pos(*a, Assoc::After);
                *b = cs.map_pos(*b, Assoc::Before).max(*a);
            }
        }
    }
    menu.mapped = version;
}

/// Keys the popup takes while it is open. Returns true if `key` was used.
pub(crate) fn handle_key(ed: &mut Editor, key: KeyEvent) -> bool {
    let Some(menu) = ed.lsp.completion.menu.as_ref() else {
        return false;
    };
    if !in_step(ed, menu) || !usable(ed) {
        close(ed);
        return false;
    }
    if menu.matches.is_empty() {
        // Nothing shown (an incomplete list waiting for a new answer).
        return false;
    }
    let plain = key.mods == Modifiers::default();
    let ctrl = |c: char| key == KeyEvent::ctrl(c);
    let step: Option<isize> = match key.code {
        KeyCode::Down if plain => Some(1),
        KeyCode::Up if plain => Some(-1),
        _ if ctrl('n') => Some(1),
        _ if ctrl('p') => Some(-1),
        _ => None,
    };
    if let Some(step) = step {
        let menu = ed.lsp.completion.menu.as_mut().expect("checked");
        let n = menu.matches.len() as isize;
        menu.selected = (menu.selected as isize + step).rem_euclid(n) as usize;
        ed.forget_recorded_key(key);
        return true;
    }
    match key.code {
        KeyCode::Tab | KeyCode::Enter if plain => {
            ed.forget_recorded_key(key);
            accept(ed);
            true
        }
        KeyCode::Esc if plain => {
            ed.forget_recorded_key(key);
            close(ed);
            true
        }
        _ => false,
    }
}

/// Hook after every key: keeps the popup in step with the text, closes it
/// when the cursor moved away, and starts requests.
pub(crate) fn after_key(
    ed: &mut Editor,
    key: KeyEvent,
    mode_before: Mode,
    doc_before: DocumentId,
    version_before: DocumentVersion,
) {
    if ed.mode != Mode::Insert || ed.prompt.is_some() || ed.picker.is_some() {
        close(ed);
        return;
    }
    if ed.is_replaying() {
        return;
    }
    let version = ed.doc().version();
    let selection = ed.view().selection.clone();
    let head = selection.primary().head;
    let text = ed.doc().text().clone();
    if let Some(menu) = ed.lsp.completion.menu.as_ref() {
        let same_place = ed.focus == menu.view && ed.view().doc == menu.doc;
        let keep = if !same_place || selection.len() != menu.selection.len() {
            false
        } else if version == menu.version {
            // No edit: moving any cursor leaves the completion.
            selection == menu.selection
        } else {
            menu.mapped == version
                && menu.start <= head
                && text.slice(menu.start..head).chars().all(is_word)
        };
        if !keep {
            close(ed);
        } else if version != menu.version {
            let menu = ed.lsp.completion.menu.as_mut().expect("checked");
            menu.head = head;
            menu.selection = selection;
            menu.version = version;
            menu.refilter(&text);
            if menu.incomplete {
                // Narrow what is shown now; the new answer replaces it.
                schedule(ed, Trigger::Incomplete);
            } else if menu.matches.is_empty() {
                close(ed);
            }
            return;
        } else {
            return;
        }
    }
    // Start a request for what was just typed.
    let Some(c) = key.typed_char() else {
        return;
    };
    let typed = mode_before == Mode::Insert
        && ed.view().doc == doc_before
        && version != version_before
        && head > 0
        && text.char(head - 1) == c;
    if !typed {
        return;
    }
    if trigger_characters(ed).contains(&c) {
        request(ed, Trigger::Character(c));
    } else if is_word(c) {
        schedule(ed, Trigger::Typing);
    }
}

/// The focused buffer's server's trigger characters.
fn trigger_characters(ed: &Editor) -> Vec<char> {
    let Some(sync) = ed.lsp.docs.get(&ed.view().doc) else {
        return Vec::new();
    };
    let Some(caps) = ed
        .lsp
        .servers
        .get(&sync.server)
        .and_then(|s| s.capabilities.as_ref())
    else {
        return Vec::new();
    };
    caps["completionProvider"]["triggerCharacters"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let s = v.as_str()?;
            let mut chars = s.chars();
            let c = chars.next()?;
            chars.next().is_none().then_some(c)
        })
        .collect()
}

/// Asks after [`DEBOUNCE`] unless another key comes first.
fn schedule(ed: &mut Editor, trigger: Trigger) {
    if !ed.lsp.enabled || !ed.lsp.docs.contains_key(&ed.view().doc) {
        return;
    }
    let token = ed.lsp.completion.debounce;
    let (view, doc) = (ed.focus, ed.view().doc);
    let version = ed.doc().version();
    let selection = ed.view().selection.clone();
    let _ = ed.spawn_background("lsp-completion-wait", move |jobs| {
        std::thread::sleep(DEBOUNCE);
        jobs.send(move |ed: &mut Editor| {
            // Only where it was scheduled: a mouse click can move focus or
            // the cursor without a key.
            let unchanged = ed.lsp.completion.debounce == token
                && ed.mode == Mode::Insert
                && ed.focus == view
                && ed.view().doc == doc
                && ed.doc().version() == version
                && ed.view().selection == selection;
            if unchanged {
                request(ed, trigger);
            }
        });
    });
}

/// Sends a completion request for the focused view's primary cursor.
/// Only a manual request reports why nothing was sent.
pub fn request(ed: &mut Editor, trigger: Trigger) -> bool {
    let manual = trigger == Trigger::Manual;
    if ed.is_replaying() {
        return false;
    }
    reconcile(ed);
    let doc = ed.view().doc;
    let Some(sync) = ed.lsp.docs.get(&doc) else {
        if manual {
            let why = match ed.doc().path() {
                None => "this buffer has no file",
                Some(p) if super::language_for(p).is_none() => {
                    "no language server for this file type"
                }
                Some(_) if !ed.lsp.enabled => "language servers are off",
                Some(_) => "the language server is not running or still starting",
            };
            ed.set_status(Severity::Warning, format!("completion: {why}"));
        }
        return false;
    };
    let Some(server) = ed.lsp.servers.get(&sync.server) else {
        return false;
    };
    if !server.supports("completionProvider") {
        if manual {
            let msg = format!("{} does not support completion", server.name);
            ed.set_status(Severity::Warning, msg);
        }
        return false;
    }
    let selection = ed.view().selection.clone();
    let params = json!({
        "textDocument": {"uri": sync.uri},
        "position": convert::char_position(ed.doc().text(), selection.primary().head),
        "context": trigger.context(),
    });
    let server_id = sync.server;
    if let Some(old) = ed.lsp.completion.pending.take()
        && let Some(s) = ed.lsp.servers.get(&old.server)
    {
        s.client.cancel(old.id);
    }
    let (id, rx) = ed.lsp.servers[&server_id]
        .client
        .request("textDocument/completion", params);
    ed.lsp.completion.next_token += 1;
    let token = ed.lsp.completion.next_token;
    ed.lsp.completion.pending = Some(Pending {
        token,
        server: server_id,
        id,
        doc,
        view: ed.focus,
        version: ed.doc().version(),
        selection,
        trigger,
    });
    let spawned = ed.spawn_background("lsp-completion", move |jobs| {
        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(reply) => {
                jobs.send(move |ed: &mut Editor| on_reply(ed, token, reply));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                jobs.send(move |ed: &mut Editor| on_timeout(ed, token));
            }
            // Canceled, superseded or the server died.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
        }
    });
    if let Err(e) = spawned {
        ed.lsp.completion.pending = None;
        ed.error(format!("completion: {e}"));
        return false;
    }
    true
}

fn on_timeout(ed: &mut Editor, token: u64) {
    let c = &mut ed.lsp.completion;
    if c.pending.as_ref().is_some_and(|p| p.token == token) {
        let p = c.pending.take().expect("checked");
        if let Some(s) = ed.lsp.servers.get(&p.server) {
            s.client.cancel(p.id);
        }
        if p.trigger == Trigger::Manual {
            ed.set_status(
                Severity::Warning,
                format!(
                    "completion: no answer within {} s",
                    REQUEST_TIMEOUT.as_secs()
                ),
            );
        }
    }
}

/// Why a reply no longer fits the editor, if it does not.
fn stale_reason(ed: &Editor, p: &Pending) -> Option<&'static str> {
    if ed.mode != Mode::Insert {
        return Some("left insert mode");
    }
    if ed.prompt.is_some() || ed.picker.is_some() {
        return Some("a prompt or picker is open");
    }
    if ed.focus != p.view || ed.view().doc != p.doc {
        return Some("another view or buffer is focused");
    }
    if ed.doc().version() != p.version {
        return Some("the buffer changed");
    }
    if ed.view().selection != p.selection {
        return Some("the cursor moved");
    }
    None
}

fn on_reply(ed: &mut Editor, token: u64, reply: Reply) {
    let c = &mut ed.lsp.completion;
    if !c.pending.as_ref().is_some_and(|p| p.token == token) {
        return; // superseded
    }
    let p = c.pending.take().expect("checked");
    let manual = p.trigger == Trigger::Manual;
    if let Some(why) = stale_reason(ed, &p) {
        ed.lsp.completion.last_dropped = Some(why.to_string());
        if manual {
            ed.info(format!("completion result dropped: {why}"));
        }
        return;
    }
    let result = match reply {
        Ok(v) => v,
        Err(e)
            if matches!(
                e.code,
                RpcError::REQUEST_CANCELLED | RpcError::CONTENT_MODIFIED
            ) =>
        {
            ed.lsp.completion.last_dropped = Some("the server canceled it".into());
            return;
        }
        Err(e) => {
            ed.lsp.completion.last_dropped = Some(format!("error: {e}"));
            if manual {
                ed.error(format!("completion: {e}"));
            }
            return;
        }
    };
    if !show(ed, &result) && manual {
        ed.info("no completions");
    }
}

/// Opens the popup at the focused view's primary cursor on a completion
/// result (protocol JSON), filtered by the identifier before the cursor.
/// Returns false, leaving no popup, when nothing matches. Replies come
/// through here after the stale-reply guard; front-end tests call it
/// directly.
pub fn show(ed: &mut Editor, result: &Value) -> bool {
    let list = proto::parse_list(result);
    let text = ed.doc().text().clone();
    let selection = ed.view().selection.clone();
    let head = selection.primary().head;
    let items: Vec<MenuItem> = list
        .items
        .iter()
        .filter_map(|item| menu_item(&text, head, item))
        .collect();
    let version = ed.doc().version();
    let mut menu = Menu {
        doc: ed.view().doc,
        view: ed.focus,
        start: word_start(&text, head),
        head,
        version,
        selection,
        mapped: version,
        items,
        matches: Vec::new(),
        selected: 0,
        incomplete: list.incomplete,
    };
    menu.refilter(&text);
    if menu.matches.is_empty() {
        ed.lsp.completion.menu = None;
        return false;
    }
    ed.lsp.completion.last_dropped = None;
    ed.lsp.completion.menu = Some(menu);
    true
}

/// Converts an item's ranges to chars. Items the editor cannot apply
/// safely are left out: a range past the end, a main edit that spans lines
/// or does not contain the cursor (the protocol requires both).
fn menu_item(text: &Rope, head: usize, item: &proto::Item) -> Option<MenuItem> {
    let (range, new_text) = match &item.edit {
        ItemEdit::Range { range, text: t } => {
            let (a, b) = convert::range_to_chars(text, range)?;
            let one_line = text.char_to_line(a) == text.char_to_line(b);
            if !(a <= head && head <= b && one_line) {
                return None;
            }
            (Some((a, b)), t.clone())
        }
        ItemEdit::Word(t) => (None, t.clone()),
    };
    let additional = item
        .additional
        .iter()
        .map(|(r, t)| {
            let (a, b) = convert::range_to_chars(text, r)?;
            Some((a, b, t.clone()))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(MenuItem {
        label: item.label.clone(),
        kind: item.kind.map_or("", proto::kind_label),
        detail: item.detail.clone().unwrap_or_default(),
        filter: item.filter_text().to_string(),
        sort: item.sort_text().to_string(),
        preselect: item.preselect,
        range,
        text: new_text,
        snippet: item.snippet,
        additional,
        stale: false,
    })
}

/// Text in the document's line ending.
fn with_line_ending(s: &str, crlf: bool) -> String {
    let lf = s.replace("\r\n", "\n");
    if crlf { lf.replace('\n', "\r\n") } else { lf }
}

/// One edit of an accepted completion, in current-text chars.
struct Edit {
    from: usize,
    to: usize,
    text: String,
    /// The cursor (selection index) this edit completes, or `None` for an
    /// additional edit.
    cursor: Option<usize>,
}

fn overlaps(a: &Edit, b: &Edit) -> bool {
    (a.from < b.to && b.from < a.to) || (a.from == b.from && a.to == b.to)
}

/// Applies the selected item as one transaction.
///
/// The primary cursor gets the item's edit. Every other cursor gets the
/// same edit, shifted, only if the text around it matches the text the
/// edit replaces at the primary (same characters before the cursor back to
/// the edit's start, and after it up to the edit's end); other cursors are
/// left alone and the status line says how many were completed.
/// `additionalTextEdits` apply once. If they overlap the main edit the item
/// is refused.
fn accept(ed: &mut Editor) {
    let Some(menu) = ed.lsp.completion.menu.take() else {
        return;
    };
    close(ed);
    let Some(item) = menu.selected_item().cloned() else {
        return;
    };
    if item.stale {
        ed.error("completion: the item is out of date; nothing was changed");
        return;
    }
    let text = ed.doc().text().clone();
    let len = text.len_chars();
    let crlf = ed.doc().line_ending() == maestro_core::LineEnding::Crlf;
    let head = menu.head;
    let (from, to) = item.range.unwrap_or((menu.start, head));
    if !(from <= head && head <= to && to <= len) {
        ed.error("completion: the item's range no longer contains the cursor");
        return;
    }
    // Snippet placeholders become plain text; the cursor goes to `$0`, or
    // to the end.
    let lf = item.text.replace("\r\n", "\n");
    let (plain, stop) = if item.snippet {
        proto::snippet_to_text(&lf)
    } else {
        (lf, None)
    };
    let stop = stop.unwrap_or_else(|| plain.chars().count());
    let new_text = with_line_ending(&plain, crlf);
    // Each `\n` before the stop gained a `\r`.
    let stop = stop
        + if crlf {
            plain.chars().take(stop).filter(|&c| c == '\n').count()
        } else {
            0
        };

    let sel = ed.view().selection.clone();
    let primary = sel.primary_index();
    let before = text.slice(from..head).to_string();
    let after = text.slice(head..to).to_string();
    let (nb, na) = (head - from, to - head);
    let mut edits = vec![Edit {
        from,
        to,
        text: new_text.clone(),
        cursor: Some(primary),
    }];
    for (a, b, t) in &item.additional {
        edits.push(Edit {
            from: *a,
            to: *b,
            text: with_line_ending(t, crlf),
            cursor: None,
        });
    }
    edits.sort_by_key(|e| (e.from, e.to));
    if edits.windows(2).any(|w| overlaps(&w[0], &w[1])) {
        ed.error("completion: the server's edits overlap; nothing was changed");
        return;
    }
    let mut skipped = 0;
    for (i, r) in sel.iter().enumerate() {
        if i == primary {
            continue;
        }
        let h = r.head;
        let fits = h >= nb
            && h + na <= len
            && text.slice(h - nb..h) == before.as_str()
            && text.slice(h..h + na) == after.as_str();
        let edit = Edit {
            from: h.saturating_sub(nb),
            to: h + na,
            text: new_text.clone(),
            cursor: Some(i),
        };
        if fits && !edits.iter().any(|e| overlaps(e, &edit)) {
            edits.push(edit);
        } else {
            skipped += 1;
        }
    }
    edits.sort_by_key(|e| (e.from, e.to));
    let changes = edits
        .iter()
        .map(|e| Change::replace(e.from, e.to, e.text.clone()));
    let cs = match ChangeSet::from_changes(len, changes) {
        Ok(cs) => cs,
        Err(e) => {
            ed.error(format!("completion: {e}"));
            return;
        }
    };
    // New cursor positions: each completed cursor sits at the stop inside
    // its inserted text; the others follow the change.
    let mut placed = vec![None; sel.len()];
    let mut delta: isize = 0;
    for e in &edits {
        let start = (e.from as isize + delta) as usize;
        if let Some(i) = e.cursor {
            placed[i] = Some(start + stop);
        }
        delta += e.text.chars().count() as isize - (e.to - e.from) as isize;
    }
    let ranges: Vec<Range> = sel
        .iter()
        .zip(&placed)
        .map(|(r, p)| Range::point(p.unwrap_or_else(|| cs.map_pos(r.head, Assoc::After))))
        .collect();
    let tx = Transaction::new(cs).with_selection(Selection::new(ranges, primary));
    if !ed.apply_or_report(tx) {
        return;
    }
    if skipped > 0 {
        let n = sel.len();
        ed.info(format!(
            "completed at {} of {n} cursors: the text before the others differs",
            n - skipped
        ));
    }
    // Replayed keys act at every cursor, so a completion that skipped
    // some cannot be repeated as typing.
    let keys = (item.additional.is_empty() && skipped == 0)
        .then(|| typed_keys(&text, from, head, to, &new_text, stop))
        .flatten();
    ed.record_completion(keys);
}

/// Keys that, typed in insert mode, make the same main edit: backspaces
/// over the replaced text, the new text, and Left back to the stop. `None`
/// when typing could not reproduce it exactly (text after the cursor is
/// replaced, whitespace or line breaks are involved, or a backspace would
/// remove a different amount than one char of the replaced text).
fn typed_keys(
    text: &Rope,
    from: usize,
    head: usize,
    to: usize,
    new_text: &str,
    stop: usize,
) -> Option<Vec<KeyEvent>> {
    if to != head {
        return None;
    }
    let simple = |c: char| !c.is_whitespace() && !c.is_control();
    if !text.slice(from..head).chars().all(simple)
        || !new_text.chars().all(|c| simple(c) || c == ' ')
    {
        return None;
    }
    let mut keys = Vec::new();
    let s = text.slice(..);
    let mut at = head;
    while at > from {
        at = prev_grapheme_boundary(s, at);
        keys.push(KeyEvent::plain(KeyCode::Backspace));
    }
    if at != from {
        return None;
    }
    keys.extend(new_text.chars().map(KeyEvent::char));
    let inserted = Rope::from_str(new_text);
    let mut at = inserted.len_chars();
    while at > stop {
        at = prev_grapheme_boundary(inserted.slice(..), at);
        keys.push(KeyEvent::plain(KeyCode::Left));
    }
    (at == stop).then_some(keys)
}
