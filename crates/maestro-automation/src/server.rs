//! Executes protocol requests against an [`Editor`].
//!
//! The [`Server`] holds what outlives a single request and is shared by
//! every connection: the per-buffer log of API edits (the first slice of
//! provenance) and the last state reported to subscribers. A [`Session`]
//! holds what belongs to one connection: whether it has initialized and
//! whether it subscribed to events.
//!
//! Every call runs on the thread that owns the editor. Transports
//! ([`crate::headless`], [`crate::socket`]) only move lines in and out.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use maestro_core::Transaction;
use maestro_core::document::{EditError, SaveError};
use maestro_core::pos::{LspPosition as CoreLsp, char_to_lsp, lsp_to_char};
use maestro_core::text::line_end_excluding_terminator;
use maestro_core::{Change, ChangeSet, Document, DocumentVersion, Range, Rope, Selection};
use maestro_editor::{DocumentId, Editor, KeyCode, KeyEvent, ViewId};
use maestro_protocol::{self as p, ErrorCode, Request, RequestId, Response, RpcError, methods};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Feature flags reported by `initialize`.
pub const CAPABILITIES: &[&str] = &[
    "apply_edit.char_positions",
    "apply_edit.lsp_positions",
    "apply_edit.version_check",
    "document.history",
    "events.document_changed",
    "events.editor_status",
];

/// Most API edits remembered per buffer; older records are dropped first.
pub const HISTORY_LIMIT: usize = 10_000;

/// Per-connection state.
#[derive(Debug, Default, Clone)]
pub struct Session {
    initialized: bool,
    subscribed: bool,
}

impl Session {
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn is_subscribed(&self) -> bool {
        self.subscribed
    }
}

pub struct Server {
    log: HashMap<DocumentId, Vec<p::EditRecord>>,
    seen_versions: HashMap<DocumentId, u64>,
    seen_status: Option<p::Status>,
}

type Res<T> = Result<T, RpcError>;

fn err(code: ErrorCode, msg: impl Into<String>) -> RpcError {
    RpcError::new(code, msg)
}

fn invalid(msg: impl Into<String>) -> RpcError {
    err(ErrorCode::InvalidParams, msg)
}

fn to_value(v: impl Serialize) -> Res<Value> {
    serde_json::to_value(v).map_err(|e| err(ErrorCode::Internal, e.to_string()))
}

/// Deserializes params; absent params count as `{}` so methods whose fields
/// all have defaults can be called without them.
fn params<T: DeserializeOwned>(v: Value) -> Res<T> {
    let v = if v.is_null() {
        Value::Object(Default::default())
    } else {
        v
    };
    serde_json::from_value(v).map_err(|e| invalid(format!("invalid params: {e}")))
}

fn io_error(context: &str, e: &io::Error) -> RpcError {
    let code = match e.kind() {
        io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        io::ErrorKind::NotFound => ErrorCode::NotFound,
        _ => ErrorCode::Io,
    };
    err(code, format!("{context}: {e}"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn mode(m: maestro_editor::Mode) -> p::Mode {
    match m {
        maestro_editor::Mode::Normal => p::Mode::Normal,
        maestro_editor::Mode::Insert => p::Mode::Insert,
        maestro_editor::Mode::Select => p::Mode::Select,
    }
}

fn status(ed: &Editor) -> Option<p::Status> {
    ed.status.as_ref().map(|(sev, msg)| p::Status {
        severity: match sev {
            maestro_editor::Severity::Info => p::Severity::Info,
            maestro_editor::Severity::Warning => p::Severity::Warning,
            maestro_editor::Severity::Error => p::Severity::Error,
        },
        message: msg.clone(),
    })
}

fn lsp(text: &Rope, c: usize) -> p::LspPosition {
    let l = char_to_lsp(text, c);
    p::LspPosition {
        line: l.line,
        character: l.character,
    }
}

fn selections(text: &Rope, sel: &Selection) -> (Vec<p::SelectionRange>, u64) {
    let ranges = sel
        .iter()
        .map(|r| p::SelectionRange {
            anchor: r.anchor as u64,
            head: r.head as u64,
            anchor_lsp: lsp(text, r.anchor),
            head_lsp: lsp(text, r.head),
        })
        .collect();
    (ranges, sel.primary_index() as u64)
}

fn encoding_name(e: p::Encoding) -> &'static str {
    match e {
        p::Encoding::Char => "char",
        p::Encoding::Lsp => "lsp",
    }
}

/// Converts a wire position to a char index, refusing positions in the
/// wrong encoding or outside the document.
fn to_char(text: &Rope, enc: p::Encoding, pos: p::Position, what: &str) -> Res<usize> {
    match (enc, pos) {
        (p::Encoding::Char, p::Position::Char(n)) => {
            let len = text.len_chars();
            match usize::try_from(n) {
                Ok(c) if c <= len => Ok(c),
                _ => Err(invalid(format!(
                    "{what} {n} is out of bounds for a document of {len} chars"
                ))),
            }
        }
        (p::Encoding::Lsp, p::Position::Lsp(l)) => {
            let line = l.line as usize;
            if line >= text.len_lines() {
                return Err(invalid(format!(
                    "{what} line {} is out of bounds for a document of {} lines",
                    l.line,
                    text.len_lines()
                )));
            }
            // Clamp the column to the line first, so a huge `character`
            // cannot overflow the addition inside `lsp_to_char` where usize
            // is 32 bits.
            let start = text.line_to_char(line);
            let end = line_end_excluding_terminator(text.slice(..), line);
            let width = text.char_to_utf16_cu(end) - text.char_to_utf16_cu(start);
            let character = (l.character as usize).min(width) as u32;
            lsp_to_char(
                text,
                CoreLsp {
                    line: l.line,
                    character,
                },
            )
            .ok_or_else(|| invalid(format!("{what} is out of bounds")))
        }
        (enc, _) => Err(invalid(format!(
            "{what} does not match encoding {:?}: char positions are numbers, lsp positions are {{\"line\", \"character\"}} objects",
            encoding_name(enc)
        ))),
    }
}

fn doc_id(ed: &Editor, doc: u64) -> Res<DocumentId> {
    let id = DocumentId(doc);
    if ed.buffers.contains_key(&id) {
        Ok(id)
    } else {
        Err(err(
            ErrorCode::NotFound,
            format!("no document with id {doc}"),
        ))
    }
}

fn view_id(ed: &Editor, view: u64) -> Res<ViewId> {
    let id = ViewId(view);
    if ed.views.contains_key(&id) {
        Ok(id)
    } else {
        Err(err(ErrorCode::NotFound, format!("no view with id {view}")))
    }
}

/// The selection that best describes "where the user is" in a document:
/// the focused view if it shows it, else any view that does, else the
/// selection remembered for when it is shown again.
fn reference_selection(ed: &Editor, id: DocumentId) -> Selection {
    if ed.view().doc == id {
        return ed.view().selection.clone();
    }
    ed.views
        .values()
        .find(|v| v.doc == id)
        .map(|v| v.selection.clone())
        .unwrap_or_else(|| ed.buffers[&id].last_selection.clone())
}

impl Server {
    /// A server whose event baseline is the editor's current state, so the
    /// first `collect_events` reports only what changed after this call.
    pub fn new(ed: &Editor) -> Self {
        let mut s = Self {
            log: HashMap::new(),
            seen_versions: HashMap::new(),
            seen_status: None,
        };
        let _ = s.collect_events(ed);
        s
    }

    /// The API edits recorded for a document, oldest first.
    pub fn history(&self, doc: DocumentId) -> &[p::EditRecord] {
        self.log.get(&doc).map(Vec::as_slice).unwrap_or_default()
    }

    /// Handles one line of input. Returns the response line, or `None` when
    /// the line was a JSON-RPC notification (no `id`).
    pub fn handle_line(
        &mut self,
        ed: &mut Editor,
        session: &mut Session,
        line: &str,
    ) -> Option<String> {
        let response = match serde_json::from_str::<Value>(line) {
            Err(e) => Some(Response::err(
                None,
                err(ErrorCode::ParseError, format!("not valid JSON: {e}")),
            )),
            Ok(v) => self.handle_value(ed, session, v),
        };
        response.map(|r| {
            serde_json::to_string(&r).unwrap_or_else(|e| {
                format!(
                    r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":{},"message":"could not encode response: {}"}}}}"#,
                    ErrorCode::Internal.code(),
                    e.to_string().replace('"', "'")
                )
            })
        })
    }

    /// The response to a line that was not valid UTF-8.
    pub fn invalid_utf8_response() -> String {
        serde_json::to_string(&Response::err(
            None,
            err(ErrorCode::ParseError, "line is not valid UTF-8"),
        ))
        .expect("static response encodes")
    }

    /// The response to a line longer than `max` bytes; the transport closes
    /// the connection after sending it.
    pub fn line_too_long_response(max: usize) -> String {
        serde_json::to_string(&Response::err(
            None,
            err(
                ErrorCode::InvalidRequest,
                format!("request line is longer than {max} bytes; closing the connection"),
            ),
        ))
        .expect("static response encodes")
    }

    fn handle_value(
        &mut self,
        ed: &mut Editor,
        session: &mut Session,
        v: Value,
    ) -> Option<Response> {
        let id: Option<RequestId> = v
            .get("id")
            .and_then(|i| serde_json::from_value(i.clone()).ok());
        if v.get("jsonrpc").and_then(Value::as_str) != Some(p::JSONRPC_VERSION) {
            return Some(Response::err(
                id,
                err(ErrorCode::InvalidRequest, "\"jsonrpc\" must be \"2.0\""),
            ));
        }
        match serde_json::from_value::<Request>(v) {
            Ok(req) => self.handle(ed, session, req),
            Err(e) => Some(Response::err(
                id,
                err(ErrorCode::InvalidRequest, format!("not a request: {e}")),
            )),
        }
    }

    /// Handles a parsed request. Returns `None` for notifications.
    pub fn handle(
        &mut self,
        ed: &mut Editor,
        session: &mut Session,
        req: Request,
    ) -> Option<Response> {
        // No `ed.poll_background()` here: on the socket transport this runs
        // inside `poll_background` already, and draining other jobs from
        // here would re-enter the connection hub's lock.
        let outcome = self.dispatch(ed, session, &req.method, req.params);
        let id = req.id?;
        Some(match outcome {
            Ok(v) => Response::ok(Some(id), v),
            Err(e) => Response::err(Some(id), e),
        })
    }

    fn dispatch(
        &mut self,
        ed: &mut Editor,
        session: &mut Session,
        method: &str,
        raw: Value,
    ) -> Res<Value> {
        if method == methods::INITIALIZE {
            return to_value(self.initialize(session, params(raw)?)?);
        }
        if !methods::ALL.contains(&method) {
            return Err(err(
                ErrorCode::MethodNotFound,
                format!("unknown method {method:?}"),
            ));
        }
        if !session.initialized {
            return Err(err(
                ErrorCode::NotInitialized,
                "call initialize before any other method",
            ));
        }
        match method {
            methods::EDITOR_SNAPSHOT => to_value(snapshot(ed)),
            methods::EDITOR_EXECUTE => to_value(execute(ed, params(raw)?)?),
            methods::EDITOR_QUIT => to_value(quit(ed, params(raw)?)?),
            methods::DOCUMENT_OPEN => to_value(open(ed, params(raw)?)?),
            methods::DOCUMENT_TEXT => to_value(text(ed, params(raw)?)?),
            methods::DOCUMENT_SAVE => to_value(save(ed, params(raw)?)?),
            methods::DOCUMENT_APPLY_EDIT => to_value(self.apply_edit(ed, params(raw)?)?),
            methods::DOCUMENT_HISTORY => {
                let p: p::DocParams = params(raw)?;
                let id = doc_id(ed, p.doc)?;
                to_value(p::HistoryResult {
                    doc: p.doc,
                    edits: self.history(id).to_vec(),
                })
            }
            methods::SELECTIONS_GET => {
                let p: p::ViewParams = params(raw)?;
                to_value(selections_result(ed, view_id(ed, p.view)?))
            }
            methods::SELECTIONS_SET => to_value(set_selections(ed, params(raw)?)?),
            methods::COMMAND_RUN => to_value(run_command(ed, params(raw)?)?),
            methods::COMMAND_LIST => to_value(list_commands(ed)),
            methods::EVENTS_SUBSCRIBE => {
                session.subscribed = true;
                to_value(p::SubscribeResult {
                    notifications: p::notifications::ALL
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                })
            }
            _ => Err(err(
                ErrorCode::Internal,
                format!("method {method:?} is listed but not dispatched"),
            )),
        }
    }

    fn initialize(
        &mut self,
        session: &mut Session,
        p: p::InitializeParams,
    ) -> Res<p::InitializeResult> {
        if session.initialized {
            return Err(err(
                ErrorCode::InvalidRequest,
                "initialize was already called on this connection",
            ));
        }
        let (major, _minor) = p::parse_version(&p.protocol_version).ok_or_else(|| {
            invalid(format!(
                "protocol_version {:?} is not MAJOR.MINOR",
                p.protocol_version
            ))
        })?;
        if major != p::PROTOCOL_MAJOR {
            return Err(err(
                ErrorCode::UnsupportedProtocolVersion,
                format!(
                    "this server speaks protocol {}; version {} is not supported",
                    p::PROTOCOL_VERSION,
                    p.protocol_version
                ),
            )
            .with_data(p::UnsupportedVersionData {
                requested: p.protocol_version,
                supported: p::PROTOCOL_VERSION.to_string(),
            }));
        }
        session.initialized = true;
        Ok(p::InitializeResult {
            protocol_version: p::PROTOCOL_VERSION.to_string(),
            server_name: "maestro".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            methods: methods::ALL.iter().map(ToString::to_string).collect(),
            notifications: p::notifications::ALL
                .iter()
                .map(ToString::to_string)
                .collect(),
            capabilities: CAPABILITIES.iter().map(ToString::to_string).collect(),
        })
    }

    /// Applies a batch of edits as one transaction and one undo step, only if
    /// the document is still at `base_version`.
    pub fn apply_edit(
        &mut self,
        ed: &mut Editor,
        p: p::ApplyEditParams,
    ) -> Res<p::ApplyEditResult> {
        if p.actor.trim().is_empty() {
            return Err(invalid("actor must not be empty"));
        }
        if p.edits.is_empty() {
            return Err(invalid("edits must not be empty"));
        }
        let id = doc_id(ed, p.doc)?;
        let doc = &ed.buffers[&id].doc;
        if let Some(r) = doc.read_only() {
            return Err(err(
                ErrorCode::ReadOnly,
                format!("document is read-only: {}", r.describe()),
            ));
        }
        let current = doc.version().0;
        if current != p.base_version {
            return Err(err(
                ErrorCode::StaleVersion,
                format!(
                    "edit was computed against version {} but the document is at {current}; nothing was changed",
                    p.base_version
                ),
            )
            .with_data(p::StaleData {
                expected: p.base_version,
                current,
            }));
        }

        let text = doc.text().clone();
        let mut changes = Vec::with_capacity(p.edits.len());
        for (i, e) in p.edits.iter().enumerate() {
            let from = to_char(
                &text,
                p.encoding,
                e.range.start,
                &format!("edits[{i}].range.start"),
            )?;
            let to = to_char(
                &text,
                p.encoding,
                e.range.end,
                &format!("edits[{i}].range.end"),
            )?;
            if from > to {
                return Err(invalid(format!(
                    "edits[{i}]: start ({from}) is after end ({to})"
                )));
            }
            changes.push((from, to, i, e.text.as_str()));
        }
        // Stable sort: insertions at the same point keep request order.
        changes.sort_by_key(|&(from, to, _, _)| (from, to));
        for w in changes.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            if b.0 < a.1 {
                return Err(invalid(format!(
                    "edits[{}] ({}..{}) and edits[{}] ({}..{}) overlap",
                    a.2, a.0, a.1, b.2, b.0, b.1
                )));
            }
        }
        let cs = ChangeSet::from_changes(
            text.len_chars(),
            changes
                .iter()
                .map(|&(from, to, _, t)| Change::replace(from, to, t)),
        )
        .map_err(|e| invalid(e.to_string()))?;
        if cs.is_empty() {
            // The version would not move, so there would be no undo step and
            // nothing to attribute.
            return Err(invalid("the edits change nothing"));
        }

        let before = reference_selection(ed, id);
        let focused = ed.focus;
        let insert_mode = ed.mode == maestro_editor::Mode::Insert;
        let buf = ed.buffers.get_mut(&id).expect("checked above");
        // Close whatever the user had open so this edit is its own undo step.
        buf.doc.commit(&before);
        buf.doc
            .apply_at_version(
                &Transaction::new(cs.clone()),
                DocumentVersion(p.base_version),
                &before,
            )
            .map_err(|e| match e {
                EditError::ReadOnly(r) => err(
                    ErrorCode::ReadOnly,
                    format!("document is read-only: {}", r.describe()),
                ),
                EditError::Stale { expected, actual } => err(
                    ErrorCode::StaleVersion,
                    format!(
                        "edit was computed against version {} but the document is at {}",
                        expected.0, actual.0
                    ),
                )
                .with_data(p::StaleData {
                    expected: expected.0,
                    current: actual.0,
                }),
                EditError::Change(c) => invalid(c.to_string()),
            })?;
        buf.last_edit = Some(Instant::now());
        let new_text = buf.doc.text().clone();
        let s = new_text.slice(..);
        let after = before.map(&cs).normalize(s);
        buf.doc.commit(&after);
        buf.last_selection = buf.last_selection.map(&cs).normalize(s).clamp_normal(s);
        let version = buf.doc.version().0;
        // An open prompt keeps the selection it started from (restored on
        // Esc, and the base of regex previews); move it with the text too.
        if ed.views.get(&focused).is_some_and(|v| v.doc == id)
            && let Some(prompt) = ed.prompt.as_mut()
        {
            prompt.original = prompt.original.map(&cs).normalize(s).clamp_normal(s);
        }
        for v in ed.views.values_mut().filter(|v| v.doc == id) {
            let mapped = v.selection.map(&cs).normalize(s);
            v.selection = if v.id == focused && insert_mode {
                mapped
            } else {
                mapped.clamp_normal(s)
            };
        }

        let log = self.log.entry(id).or_default();
        log.push(p::EditRecord {
            actor: p.actor,
            description: p.description,
            base_version: p.base_version,
            new_version: version,
            timestamp_ms: now_ms(),
        });
        if log.len() > HISTORY_LIMIT {
            let excess = log.len() - HISTORY_LIMIT;
            log.drain(..excess);
        }
        Ok(p::ApplyEditResult {
            doc: p.doc,
            base_version: p.base_version,
            version,
        })
    }

    /// Notifications for what changed since the last call: document versions
    /// and the status line. Changes between calls are coalesced.
    pub fn collect_events(&mut self, ed: &Editor) -> Vec<p::Notification> {
        let mut out = Vec::new();
        self.seen_versions
            .retain(|id, _| ed.buffers.contains_key(id));
        self.log.retain(|id, _| ed.buffers.contains_key(id));
        for (id, b) in &ed.buffers {
            let version = b.doc.version().0;
            match self.seen_versions.insert(*id, version) {
                Some(old) if old != version => {
                    let actor = self
                        .log
                        .get(id)
                        .and_then(|l| l.last())
                        .filter(|r| r.new_version == version)
                        .map(|r| r.actor.clone());
                    out.push(p::Notification::new(
                        p::notifications::DOCUMENT_CHANGED,
                        p::DocumentChanged {
                            doc: id.0,
                            version,
                            actor,
                        },
                    ));
                }
                _ => {}
            }
        }
        let st = status(ed);
        if st != self.seen_status {
            self.seen_status = st.clone();
            out.push(p::Notification::new(
                p::notifications::EDITOR_STATUS,
                p::StatusChanged { status: st },
            ));
        }
        out
    }
}

// ----- editor methods ---------------------------------------------------------

/// The `editor.snapshot` result.
pub fn snapshot(ed: &Editor) -> p::SnapshotResult {
    let buffers = ed
        .buffers
        .values()
        .map(|b| {
            let t = b.doc.text();
            p::BufferInfo {
                id: b.id.0,
                path: b.doc.path().map(|p| p.display().to_string()),
                name: b.doc.display_name(),
                version: b.doc.version().0,
                modified: b.doc.is_modified(),
                read_only: b.doc.read_only().map(|r| r.describe().to_string()),
                line_count: maestro_editor::motion::last_line(t.slice(..)) as u64 + 1,
                char_count: t.len_chars() as u64,
            }
        })
        .collect();
    let views = ed
        .views
        .values()
        .map(|v| {
            let (selections, primary) = selections(ed.buffers[&v.doc].doc.text(), &v.selection);
            p::ViewInfo {
                id: v.id.0,
                doc: v.doc.0,
                selections,
                primary,
                top_line: v.top_line as u64,
            }
        })
        .collect();
    p::SnapshotResult {
        buffers,
        views,
        focused_view: ed.focus.0,
        mode: mode(ed.mode),
        status: status(ed),
    }
}

fn interactive_state(ed: &Editor) -> (bool, bool, bool) {
    (
        ed.prompt.is_some(),
        ed.picker.is_some(),
        ed.pending_char_label().is_some(),
    )
}

/// Runs `f`, then refuses if it opened a prompt, picker or character
/// request (the API cannot type into them; they are canceled with Esc so
/// the editor is not left waiting), or if it reported an error.
fn run_noninteractive(
    ed: &mut Editor,
    what: &str,
    f: impl FnOnce(&mut Editor),
) -> Res<p::CommandResult> {
    let before = interactive_state(ed);
    // Running a command while the user is mid-input would mix the two: an
    // Esc meant to cancel this command would close the user's prompt.
    if before.0 || before.1 || before.2 || !ed.pending.is_empty() || ed.count.is_some() {
        return Err(err(
            ErrorCode::CommandFailed,
            format!(
                "{what} was not run: the editor is waiting for the user's input (a prompt, picker, typed character, count or partial key sequence)"
            ),
        ));
    }
    ed.status = None;
    f(ed);
    let after = interactive_state(ed);
    let opened = [
        (after.0 && !before.0, "a prompt"),
        (after.1 && !before.1, "a picker"),
        (after.2 && !before.2, "a typed character"),
    ]
    .into_iter()
    .find(|(opened, _)| *opened)
    .map(|(_, what)| what);
    if let Some(input) = opened {
        ed.handle_key(KeyEvent::plain(KeyCode::Esc));
        return Err(err(
            ErrorCode::CommandFailed,
            format!(
                "{what} needs interactive input ({input}), which the automation API cannot supply; it was canceled"
            ),
        ));
    }
    if let Some((maestro_editor::Severity::Error, msg)) = &ed.status {
        return Err(err(ErrorCode::CommandFailed, msg.clone()));
    }
    Ok(p::CommandResult {
        mode: mode(ed.mode),
        status: status(ed),
    })
}

fn execute(ed: &mut Editor, p: p::ExecuteParams) -> Res<p::CommandResult> {
    let line = p.line.trim().trim_start_matches(':').to_string();
    if line.is_empty() {
        return Err(invalid("line is empty"));
    }
    run_noninteractive(ed, &format!(":{line}"), |ed| ed.execute_line(&line))
}

fn run_command(ed: &mut Editor, p: p::CommandRunParams) -> Res<p::CommandResult> {
    let name = p.name.as_str();
    if maestro_editor::commands::lookup(name).is_none()
        && maestro_editor::typed::lookup(name).is_none()
    {
        return Err(err(
            ErrorCode::NotFound,
            format!("no command named {name:?}"),
        ));
    }
    if p.count == Some(0) {
        return Err(invalid("count must be at least 1"));
    }
    let count = p.count.map(|n| n.min(1_000_000) as usize);
    run_noninteractive(ed, name, |ed| {
        ed.count = count;
        ed.run_command(name);
        if ed.pending_char_label().is_none() {
            ed.count = None;
            ed.selected_register = None;
        }
        // The same bookkeeping a key press gets.
        if ed.mode != maestro_editor::Mode::Insert {
            ed.commit();
            let text = ed.doc().text().clone();
            let sel = ed.view().selection.clamp_normal(text.slice(..));
            ed.view_mut().selection = sel;
        }
        ed.ensure_cursor_visible();
    })
}

fn quit(ed: &mut Editor, p: p::QuitParams) -> Res<p::QuitResult> {
    let modified = ed.modified_buffers();
    if !p.force && !modified.is_empty() {
        return Err(err(
            ErrorCode::UnsavedChanges,
            format!(
                "unsaved changes in {}; save them or pass force",
                modified.join(", ")
            ),
        )
        .with_data(modified));
    }
    ed.should_quit = true;
    Ok(p::QuitResult {})
}

fn list_commands(ed: &Editor) -> p::CommandListResult {
    let mut keys: HashMap<String, Vec<p::KeyBinding>> = HashMap::new();
    for m in [
        maestro_editor::Mode::Normal,
        maestro_editor::Mode::Select,
        maestro_editor::Mode::Insert,
    ] {
        for (k, cmd) in ed.keymaps.bindings(m) {
            keys.entry(cmd).or_default().push(p::KeyBinding {
                mode: mode(m),
                keys: k,
            });
        }
    }
    let mut commands: Vec<p::CommandInfo> = maestro_editor::commands::all()
        .map(|c| p::CommandInfo {
            name: c.name.to_string(),
            kind: p::CommandKind::Command,
            doc: c.doc.to_string(),
            aliases: Vec::new(),
            keys: keys.get(c.name).cloned().unwrap_or_default(),
        })
        .collect();
    commands.extend(maestro_editor::typed::all().iter().map(|c| p::CommandInfo {
        name: c.name.to_string(),
        kind: p::CommandKind::Typed,
        doc: c.doc.to_string(),
        aliases: c.aliases.iter().map(ToString::to_string).collect(),
        keys: keys.get(c.name).cloned().unwrap_or_default(),
    }));
    p::CommandListResult { commands }
}

// ----- documents ------------------------------------------------------------

fn open(ed: &mut Editor, p: p::DocumentOpenParams) -> Res<p::DocumentOpenResult> {
    if p.path.is_empty() {
        return Err(invalid("path is empty"));
    }
    let path = ed.resolve_path(Path::new(&p.path));
    if path.is_dir() {
        return Err(invalid(format!("{} is a directory", path.display())));
    }
    // Reading a FIFO, socket or device could block the editor thread with
    // no way to cancel. A missing file is fine: it is created on save.
    if let Ok(m) = std::fs::metadata(&path)
        && !m.is_file()
    {
        return Err(invalid(format!("{} is not a regular file", path.display())));
    }
    let (id, already_open) = match ed.find_buffer_by_path(&path) {
        Some(id) => (id, true),
        None => {
            let doc =
                Document::open(&path).map_err(|e| io_error(&path.display().to_string(), &e))?;
            let id = ed.add_document(doc);
            maestro_editor::recovery::notify_if_recoverable(ed, id);
            (id, false)
        }
    };
    if p.focus {
        ed.switch_to(id);
    }
    let doc = &ed.buffers[&id].doc;
    Ok(p::DocumentOpenResult {
        doc: id.0,
        path: doc
            .path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        version: doc.version().0,
        read_only: doc.read_only().map(|r| r.describe().to_string()),
        already_open,
    })
}

fn text(ed: &Editor, p: p::DocumentTextParams) -> Res<p::DocumentTextResult> {
    let id = doc_id(ed, p.doc)?;
    let doc = &ed.buffers[&id].doc;
    let t = doc.text();
    let text = match p.range {
        None => t.to_string(),
        Some(r) => {
            let enc = p
                .encoding
                .ok_or_else(|| invalid("encoding is required when range is given"))?;
            let from = to_char(t, enc, r.start, "range.start")?;
            let to = to_char(t, enc, r.end, "range.end")?;
            if from > to {
                return Err(invalid(format!("range start ({from}) is after end ({to})")));
            }
            t.slice(from..to).to_string()
        }
    };
    Ok(p::DocumentTextResult {
        doc: p.doc,
        version: doc.version().0,
        text,
    })
}

fn save(ed: &mut Editor, p: p::DocumentSaveParams) -> Res<p::DocumentSaveResult> {
    let id = doc_id(ed, p.doc)?;
    let sel = reference_selection(ed, id);
    let recovery_dir = ed.recovery_dir.clone();
    let buf = ed.buffers.get_mut(&id).expect("checked above");
    match buf.doc.save(&sel, p.force) {
        Ok(()) => {
            buf.disk_change_reported = false;
            buf.recovery_version = None;
            let path = buf.doc.path().map(Path::to_path_buf).unwrap_or_default();
            if let Some(dir) = recovery_dir {
                maestro_editor::recovery::remove(&dir, &path);
            }
            Ok(p::DocumentSaveResult {
                doc: p.doc,
                path: path.display().to_string(),
                version: buf.doc.version().0,
            })
        }
        Err(SaveError::NoPath) => Err(invalid("document has no path; it is a scratch buffer")),
        Err(SaveError::ReadOnly(r)) => Err(err(
            ErrorCode::ReadOnly,
            format!("document is read-only: {}", r.describe()),
        )),
        Err(e @ SaveError::ExternallyModified(_)) => Err(err(ErrorCode::Conflict, e.to_string())),
        Err(SaveError::Io(e)) => Err(io_error("save failed", &e)),
    }
}

// ----- selections ------------------------------------------------------------

fn selections_result(ed: &Editor, id: ViewId) -> p::SelectionsResult {
    let v = &ed.views[&id];
    let doc = &ed.buffers[&v.doc].doc;
    let (selections, primary) = selections(doc.text(), &v.selection);
    p::SelectionsResult {
        view: id.0,
        doc: v.doc.0,
        version: doc.version().0,
        selections,
        primary,
    }
}

fn set_selections(ed: &mut Editor, p: p::SelectionsSetParams) -> Res<p::SelectionsResult> {
    let id = view_id(ed, p.view)?;
    if p.ranges.is_empty() {
        return Err(invalid("ranges must not be empty"));
    }
    let doc_id = ed.views[&id].doc;
    let text = ed.buffers[&doc_id].doc.text().clone();
    let mut ranges = Vec::with_capacity(p.ranges.len());
    for (i, r) in p.ranges.iter().enumerate() {
        let anchor = to_char(&text, p.encoding, r.anchor, &format!("ranges[{i}].anchor"))?;
        let head = to_char(&text, p.encoding, r.head, &format!("ranges[{i}].head"))?;
        ranges.push(Range::new(anchor, head));
    }
    let primary = match p.primary {
        None => ranges.len() - 1,
        Some(i) if (i as usize) < ranges.len() => i as usize,
        Some(i) => {
            return Err(invalid(format!(
                "primary {i} is out of bounds for {} ranges",
                ranges.len()
            )));
        }
    };
    let s = text.slice(..);
    let mut sel = Selection::new(ranges, primary).normalize(s);
    let focused = ed.focus == id;
    if !(focused && ed.mode == maestro_editor::Mode::Insert) {
        sel = sel.clamp_normal(s);
    }
    let v = ed.views.get_mut(&id).expect("checked above");
    v.selection = sel;
    v.sticky_cols = None;
    if focused {
        ed.ensure_cursor_visible();
    }
    Ok(selections_result(ed, id))
}
