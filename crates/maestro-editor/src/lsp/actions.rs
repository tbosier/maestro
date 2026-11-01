//! Rename, formatting, code actions and server commands: requests whose
//! answers change text.
//!
//! One such request is out at a time; a new one cancels the old. Replies
//! come back as jobs like every other request (see [`super`]) and pass a
//! guard first: a reply that would edit text is dropped if any buffer's
//! version moved since the request went out, and one that would open a
//! prompt or a picker is also dropped if the user moved to another buffer,
//! a prompt, a picker or insert mode meanwhile. Edits then go through
//! [`super::edit`], which checks the whole edit before changing anything.
//!
//! * Rename asks `textDocument/prepareRename` first when the server offers
//!   it, opens a prompt with the current name, and applies the
//!   `WorkspaceEdit` from `textDocument/rename`.
//! * Formatting applies `textDocument/formatting` (or `rangeFormatting`
//!   for the primary selection) to the buffer as one undo step, keeping
//!   the cursor's line at the same screen row.
//! * Code actions list `textDocument/codeAction` for the primary selection,
//!   with the diagnostics that overlap it, in a picker. A chosen action's
//!   edit is applied, then its command run with `workspace/executeCommand`.
//!   An action without either is completed with `codeAction/resolve` when
//!   the server can.

use std::collections::HashMap;

use maestro_core::DocumentVersion;
use maestro_lsp::convert::{self, char_position};
use maestro_lsp::edit::{DocumentEdit, parse_text_edits};
use maestro_lsp::{RequestId, RpcError, Value, json};

use super::edit::{apply_document_edits, apply_workspace_edit, plural};
use super::{REQUEST_TIMEOUT, ServerId};
use crate::editor::{DocumentId, Editor, Mode, Severity, ViewId};
use crate::picker::{Picker, PickerAction, PickerItem};
use crate::prompt::PromptKind;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    PrepareRename,
    Rename { new_name: String },
    Format,
    CodeActions,
    Resolve { title: String },
    Execute { title: String },
}

impl Action {
    fn label(&self) -> &'static str {
        match self {
            Action::PrepareRename | Action::Rename { .. } => "rename",
            Action::Format => "format",
            Action::CodeActions | Action::Resolve { .. } => "code action",
            Action::Execute { .. } => "command",
        }
    }

    /// Replies that open a prompt or picker, which must not appear over
    /// whatever the user turned to.
    fn opens_ui(&self) -> bool {
        matches!(self, Action::PrepareRename | Action::CodeActions)
    }
}

struct Pending {
    token: u64,
    server: ServerId,
    id: RequestId,
    action: Action,
    doc: DocumentId,
    view: ViewId,
    /// Every buffer's version when the request went out.
    versions: HashMap<DocumentId, DocumentVersion>,
    /// The primary cursor then.
    head: usize,
}

/// The code actions a picker shows.
struct Menu {
    server: ServerId,
    versions: HashMap<DocumentId, DocumentVersion>,
    actions: Vec<Value>,
}

/// Where a rename prompt points.
struct RenameTarget {
    server: ServerId,
    doc: DocumentId,
    version: DocumentVersion,
    head: usize,
}

/// State for these requests, part of [`super::Lsp`].
#[derive(Default)]
pub struct Actions {
    pending: Option<Pending>,
    menu: Option<Menu>,
    rename: Option<RenameTarget>,
    next_token: u64,
    /// Why the last reply was not used, for tests and diagnosis.
    pub last_dropped: Option<String>,
}

impl Actions {
    /// True while a request is out.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

fn versions(ed: &Editor) -> HashMap<DocumentId, DocumentVersion> {
    ed.buffers
        .iter()
        .map(|(id, b)| (*id, b.doc.version()))
        .collect()
}

fn changed_since(ed: &Editor, versions: &HashMap<DocumentId, DocumentVersion>) -> bool {
    versions
        .iter()
        .any(|(id, v)| ed.buffers.get(id).is_some_and(|b| b.doc.version() != *v))
}

/// Cancels the request that is out, if any.
pub fn cancel(ed: &mut Editor) {
    if let Some(p) = ed.lsp.actions.pending.take()
        && let Some(s) = ed.lsp.servers.get(&p.server)
    {
        s.client.cancel(p.id);
    }
}

/// Forgets state tied to a server that stopped.
pub(super) fn server_stopped(ed: &mut Editor, server: ServerId) {
    let a = &mut ed.lsp.actions;
    if a.pending.as_ref().is_some_and(|p| p.server == server) {
        a.pending = None;
    }
    if a.menu.as_ref().is_some_and(|m| m.server == server) {
        a.menu = None;
    }
    if a.rename.as_ref().is_some_and(|r| r.server == server) {
        a.rename = None;
    }
}

/// Forgets everything (`:lsp-restart`).
pub(super) fn reset(ed: &mut Editor) {
    let a = &mut ed.lsp.actions;
    a.pending = None;
    a.menu = None;
    a.rename = None;
}

/// Sends `method` to `server` and handles the reply as `action`.
fn send(ed: &mut Editor, server: ServerId, method: &str, params: Value, action: Action) -> bool {
    cancel(ed);
    let Some(s) = ed.lsp.servers.get(&server) else {
        return false;
    };
    let (id, rx) = s.client.request(method, params);
    ed.lsp.actions.next_token += 1;
    let token = ed.lsp.actions.next_token;
    let label = action.label();
    ed.lsp.actions.pending = Some(Pending {
        token,
        server,
        id,
        action,
        doc: ed.view().doc,
        view: ed.focus,
        versions: versions(ed),
        head: ed.view().selection.primary().head,
    });
    let spawned = ed.spawn_background("lsp-action", move |jobs| {
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
        ed.lsp.actions.pending = None;
        ed.error(format!("{label}: {e}"));
        return false;
    }
    true
}

fn on_timeout(ed: &mut Editor, token: u64) {
    if !ed
        .lsp
        .actions
        .pending
        .as_ref()
        .is_some_and(|p| p.token == token)
    {
        return;
    }
    let p = ed.lsp.actions.pending.take().expect("checked");
    if let Some(s) = ed.lsp.servers.get(&p.server) {
        s.client.cancel(p.id);
    }
    ed.set_status(
        Severity::Warning,
        format!(
            "{}: no answer within {} s",
            p.action.label(),
            REQUEST_TIMEOUT.as_secs()
        ),
    );
}

/// Why a reply no longer fits the editor, if it does not.
fn stale_reason(ed: &Editor, p: &Pending) -> Option<&'static str> {
    if matches!(p.action, Action::Execute { .. }) {
        // Only reports success or failure; edits come separately.
        return None;
    }
    if changed_since(ed, &p.versions) {
        return Some("a buffer changed while waiting");
    }
    if p.action.opens_ui() {
        if ed.focus != p.view || ed.view().doc != p.doc {
            return Some("another buffer is shown now");
        }
        if ed.prompt.is_some() || ed.picker.is_some() {
            return Some("a prompt or picker is open");
        }
        if ed.mode == Mode::Insert {
            return Some("you were typing");
        }
    }
    None
}

fn on_reply(ed: &mut Editor, token: u64, reply: maestro_lsp::Reply) {
    if !ed
        .lsp
        .actions
        .pending
        .as_ref()
        .is_some_and(|p| p.token == token)
    {
        return; // superseded
    }
    let p = ed.lsp.actions.pending.take().expect("checked");
    let label = p.action.label();
    if let Some(why) = stale_reason(ed, &p) {
        ed.lsp.actions.last_dropped = Some(why.to_string());
        ed.info(format!(
            "{label} result dropped: {why}; nothing was changed"
        ));
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
            ed.lsp.actions.last_dropped = Some("the server canceled it".into());
            ed.info(format!("{label}: the server canceled the request"));
            return;
        }
        Err(e) => {
            ed.lsp.actions.last_dropped = Some(format!("error: {e}"));
            ed.error(format!("{label}: {}", e.message));
            return;
        }
    };
    ed.lsp.actions.last_dropped = None;
    match p.action.clone() {
        Action::PrepareRename => prepared(ed, &p, &result),
        Action::Rename { new_name } => renamed(ed, &new_name, &result),
        Action::Format => formatted(ed, &p, &result),
        Action::CodeActions => listed(ed, &p, result),
        Action::Resolve { title } => run_resolved(ed, p.server, &title, &result),
        Action::Execute { title } => {
            // A command's own edits arrive as `workspace/applyEdit` and set
            // the status line; only say something if they did not.
            if ed.status.is_none() {
                ed.info(format!("ran {title}"));
            }
        }
    }
}

// ----- rename ------------------------------------------------------------------

/// Renames the symbol at the cursor. With `new_name`, at once; otherwise
/// through a prompt prefilled with the current name.
pub fn rename(ed: &mut Editor, new_name: Option<&str>) {
    let Some((server, uri)) = super::focused_server(ed, "rename", "renameProvider") else {
        return;
    };
    let head = ed.view().selection.primary().head;
    if let Some(name) = new_name {
        let doc = ed.view().doc;
        let version = ed.doc().version();
        send_rename(
            ed,
            RenameTarget {
                server,
                doc,
                version,
                head,
            },
            name,
        );
        return;
    }
    let prepare = ed.lsp.servers[&server]
        .capabilities
        .as_ref()
        .is_some_and(|c| c["renameProvider"]["prepareProvider"] == json!(true));
    if prepare {
        let params = json!({
            "textDocument": {"uri": uri},
            "position": char_position(ed.doc().text(), head),
        });
        send(
            ed,
            server,
            "textDocument/prepareRename",
            params,
            Action::PrepareRename,
        );
    } else {
        let word = word_at(ed, head);
        if word.is_empty() {
            ed.error("rename: no name under the cursor");
            return;
        }
        open_rename_prompt(ed, server, &word);
    }
}

/// The identifier around char `head` in the focused buffer.
fn word_at(ed: &Editor, head: usize) -> String {
    let text = ed.doc().text();
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let len = text.len_chars();
    let mut start = head.min(len);
    while start > 0 && is_word(text.char(start - 1)) {
        start -= 1;
    }
    let mut end = head.min(len);
    while end < len && is_word(text.char(end)) {
        end += 1;
    }
    text.slice(start..end).to_string()
}

fn prepared(ed: &mut Editor, p: &Pending, result: &Value) {
    let text = ed.doc().text().clone();
    let name = match result {
        Value::Null => {
            ed.error("rename: the server says nothing here can be renamed");
            return;
        }
        v if v.get("defaultBehavior").is_some() => word_at(ed, p.head),
        v if v.get("placeholder").is_some() => {
            v["placeholder"].as_str().unwrap_or_default().to_string()
        }
        v => match convert::range_to_chars(&text, v) {
            Some((a, b)) => text.slice(a..b).to_string(),
            None => word_at(ed, p.head),
        },
    };
    open_rename_prompt(ed, p.server, &name);
}

fn open_rename_prompt(ed: &mut Editor, server: ServerId, name: &str) {
    ed.lsp.actions.rename = Some(RenameTarget {
        server,
        doc: ed.view().doc,
        version: ed.doc().version(),
        head: ed.view().selection.primary().head,
    });
    crate::prompt::open(ed, PromptKind::Rename);
    if let Some(p) = ed.prompt.as_mut() {
        p.input = name.to_string();
        p.cursor = p.input.len();
    }
}

/// The rename prompt was accepted with `input`.
pub(crate) fn rename_prompt_done(ed: &mut Editor, input: &str) {
    let Some(target) = ed.lsp.actions.rename.take() else {
        return;
    };
    let name = input.trim();
    if name.is_empty() {
        ed.info("rename canceled: no name given");
        return;
    }
    send_rename(ed, target, name);
}

fn send_rename(ed: &mut Editor, target: RenameTarget, name: &str) {
    let current = ed.buffers.get(&target.doc).map(|b| b.doc.version());
    if current != Some(target.version) {
        ed.error("rename canceled: the buffer changed; nothing was changed");
        return;
    }
    let Some(sync) = ed.lsp.docs.get(&target.doc) else {
        ed.error("rename: the buffer is no longer open on the language server");
        return;
    };
    let text = ed.buffers[&target.doc].doc.text();
    let params = json!({
        "textDocument": {"uri": sync.uri},
        "position": char_position(text, target.head),
        "newName": name,
    });
    send(
        ed,
        target.server,
        "textDocument/rename",
        params,
        Action::Rename {
            new_name: name.to_string(),
        },
    );
}

fn renamed(ed: &mut Editor, name: &str, result: &Value) {
    if result.is_null() {
        ed.info("rename: the server made no changes");
        return;
    }
    match apply_workspace_edit(ed, result) {
        Ok(a) if a.changed.is_empty() => ed.info("rename: the server made no changes"),
        Ok(a) => {
            let msg = format!(
                "renamed to {name}: {} in {}",
                plural(a.edits, "edit"),
                a.summary()
            );
            let sev = if a.warnings.is_empty() {
                Severity::Info
            } else {
                Severity::Warning
            };
            ed.set_status(sev, msg);
        }
        Err(e) => ed.error(format!("rename refused, nothing was changed: {e}")),
    }
}

// ----- formatting --------------------------------------------------------------

/// Formats the focused buffer, or with `selection` only the primary
/// selection.
pub fn format(ed: &mut Editor, selection: bool) {
    let (what, capability) = if selection {
        ("range formatting", "documentRangeFormattingProvider")
    } else {
        ("formatting", "documentFormattingProvider")
    };
    let Some((server, uri)) = super::focused_server(ed, what, capability) else {
        return;
    };
    let cfg = &ed.config.editor;
    let mut params = json!({
        "textDocument": {"uri": uri},
        "options": {
            "tabSize": cfg.indent_width.max(1),
            "insertSpaces": cfg.indent == maestro_config::IndentStyle::Spaces,
        },
    });
    let method = if selection {
        let text = ed.doc().text();
        let r = ed.view().selection.primary();
        params["range"] = json!({
            "start": char_position(text, r.from()),
            "end": char_position(text, r.to(text.slice(..))),
        });
        "textDocument/rangeFormatting"
    } else {
        "textDocument/formatting"
    };
    send(ed, server, method, params, Action::Format);
}

fn formatted(ed: &mut Editor, p: &Pending, result: &Value) {
    let edits = match parse_text_edits(result) {
        Ok(e) => e,
        Err(e) => {
            ed.error(format!("format: {e}"));
            return;
        }
    };
    let Some(sync) = ed.lsp.docs.get(&p.doc) else {
        ed.error("format: the buffer is no longer open on the language server");
        return;
    };
    let edit = DocumentEdit {
        uri: sync.uri.clone(),
        version: Some(p.versions[&p.doc].0 as i64),
        edits,
    };
    // Keep the cursor's line at the same screen row in each view of the
    // buffer, so formatting does not make the view jump.
    let rows: Vec<(ViewId, usize, usize)> = ed
        .views
        .values()
        .filter(|v| v.doc == p.doc)
        .map(|v| {
            let text = ed.buffers[&v.doc].doc.text();
            let line = text.char_to_line(v.selection.primary().head.min(text.len_chars()));
            (v.id, line, v.top_line)
        })
        .collect();
    match apply_document_edits(ed, vec![edit], true) {
        Ok(a) if a.changed.is_empty() => ed.info("format: already formatted"),
        Ok(a) => {
            for (id, line, top) in rows {
                let text = ed.buffers[&p.doc].doc.text().clone();
                if let Some(v) = ed.views.get_mut(&id) {
                    let now = text.char_to_line(v.selection.primary().head.min(text.len_chars()));
                    v.top_line = (top + now)
                        .saturating_sub(line)
                        .min(text.len_lines().saturating_sub(1));
                }
            }
            ed.ensure_cursor_visible();
            ed.info(format!("formatted: {}, not saved", plural(a.edits, "edit")));
        }
        Err(e) => ed.error(format!("format refused, nothing was changed: {e}")),
    }
}

// ----- code actions ------------------------------------------------------------

/// Lists the code actions for the primary selection (or the cursor) in a
/// picker.
pub fn code_actions(ed: &mut Editor) {
    let Some((server, uri)) = super::focused_server(ed, "code actions", "codeActionProvider")
    else {
        return;
    };
    let doc = ed.view().doc;
    let text = ed.doc().text().clone();
    let r = ed.view().selection.primary();
    // A cursor is an empty range; a selection covers its characters.
    let (from, to) = if r.anchor == r.head {
        (r.head, r.head)
    } else {
        (r.from(), r.to(text.slice(..)))
    };
    let diagnostics: Vec<Value> = ed
        .lsp
        .diagnostics(doc)
        .iter()
        .filter(|d| d.from <= to && d.to >= from)
        .map(|d| {
            let mut v = if d.raw.is_object() {
                d.raw.clone()
            } else {
                json!({"message": d.message})
            };
            // The range as it is now, after any edits since publishing.
            v["range"] = json!({
                "start": char_position(&text, d.from.min(text.len_chars())),
                "end": char_position(&text, d.to.min(text.len_chars())),
            });
            v
        })
        .collect();
    let params = json!({
        "textDocument": {"uri": uri},
        "range": {"start": char_position(&text, from), "end": char_position(&text, to)},
        "context": {"diagnostics": diagnostics, "triggerKind": 1},
    });
    send(
        ed,
        server,
        "textDocument/codeAction",
        params,
        Action::CodeActions,
    );
}

fn listed(ed: &mut Editor, p: &Pending, result: Value) {
    let actions: Vec<Value> = match result {
        Value::Array(items) => items
            .into_iter()
            .filter(|a| a.get("title").and_then(Value::as_str).is_some())
            .collect(),
        _ => Vec::new(),
    };
    if actions.is_empty() {
        ed.info("no code actions here");
        return;
    }
    let items = actions
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let mut detail = Vec::new();
            if let Some(kind) = a
                .get("kind")
                .and_then(Value::as_str)
                .filter(|k| !k.is_empty())
            {
                detail.push(kind.to_string());
            }
            if a["isPreferred"] == json!(true) {
                detail.push("preferred".into());
            }
            if let Some(reason) = a["disabled"]["reason"].as_str() {
                detail.push(format!("unavailable: {reason}"));
            }
            PickerItem {
                label: a["title"].as_str().unwrap_or_default().to_string(),
                detail: detail.join(", "),
                action: PickerAction::CodeAction(i),
            }
        })
        .collect();
    ed.lsp.actions.menu = Some(Menu {
        server: p.server,
        versions: p.versions.clone(),
        actions,
    });
    ed.picker = Some(Picker::new("code actions", items));
}

/// Runs the code action at `index` of the last list (the picker's choice).
pub(crate) fn run_code_action(ed: &mut Editor, index: usize) {
    let Some(menu) = ed.lsp.actions.menu.take() else {
        ed.error("code action: the list is gone; ask again");
        return;
    };
    let Some(action) = menu.actions.get(index).cloned() else {
        return;
    };
    if !ed.lsp.servers.contains_key(&menu.server) {
        ed.error("code action: the language server stopped");
        return;
    }
    if changed_since(ed, &menu.versions) {
        ed.error("code action not applied: a buffer changed since the list was made; ask again");
        return;
    }
    let title = action["title"].as_str().unwrap_or_default().to_string();
    if let Some(reason) = action["disabled"]["reason"].as_str() {
        ed.info(format!("{title} is unavailable: {reason}"));
        return;
    }
    // A bare `Command`.
    if action["command"].is_string() {
        execute(ed, menu.server, &action, &title);
        return;
    }
    let has_edit = action.get("edit").is_some_and(Value::is_object);
    let has_command = action.get("command").is_some_and(Value::is_object);
    let can_resolve = ed.lsp.servers[&menu.server]
        .capabilities
        .as_ref()
        .is_some_and(|c| c["codeActionProvider"]["resolveProvider"] == json!(true));
    if !has_edit && can_resolve {
        send(
            ed,
            menu.server,
            "codeAction/resolve",
            action,
            Action::Resolve { title },
        );
        return;
    }
    if !has_edit && !has_command {
        ed.error(format!("{title}: the server gave no edit or command"));
        return;
    }
    run_resolved(ed, menu.server, &title, &action);
}

/// Applies a code action's edit, then runs its command.
fn run_resolved(ed: &mut Editor, server: ServerId, title: &str, action: &Value) {
    let mut done = None;
    if let Some(edit) = action.get("edit").filter(|e| e.is_object()) {
        match apply_workspace_edit(ed, edit) {
            Ok(a) => done = Some(a),
            Err(e) => {
                ed.error(format!("{title}: refused, nothing was changed: {e}"));
                return;
            }
        }
    }
    match &done {
        Some(a) if !a.changed.is_empty() => {
            let sev = if a.warnings.is_empty() {
                Severity::Info
            } else {
                Severity::Warning
            };
            ed.set_status(sev, format!("{title}: {}", a.summary()));
        }
        _ => {}
    }
    match action.get("command").filter(|c| c.is_object()) {
        Some(cmd) => execute(ed, server, cmd, title),
        None if done.as_ref().is_none_or(|a| a.changed.is_empty()) => {
            ed.info(format!("{title}: no changes"));
        }
        None => {}
    }
}

/// Runs a `Command` on the server.
fn execute(ed: &mut Editor, server: ServerId, cmd: &Value, title: &str) {
    let Some(name) = cmd["command"].as_str() else {
        ed.error(format!("{title}: malformed command"));
        return;
    };
    let mut params = json!({"command": name});
    if let Some(args) = cmd.get("arguments").filter(|a| !a.is_null()) {
        params["arguments"] = args.clone();
    }
    send(
        ed,
        server,
        "workspace/executeCommand",
        params,
        Action::Execute {
            title: title.to_string(),
        },
    );
}

// ----- edits the server asks for ----------------------------------------------

/// Answers `workspace/applyEdit`: applies the edit (all or nothing) and
/// says whether it was applied.
pub(super) fn apply_edit_request(ed: &mut Editor, server: ServerId, params: &Value) -> Value {
    let name = ed
        .lsp
        .servers
        .get(&server)
        .map_or_else(String::new, |s| s.name.clone());
    let label = params["label"].as_str().unwrap_or("edit");
    match apply_workspace_edit(ed, &params["edit"]) {
        Ok(a) => {
            if !a.changed.is_empty() {
                let sev = if a.warnings.is_empty() {
                    Severity::Info
                } else {
                    Severity::Warning
                };
                ed.set_status(sev, format!("{name} {label}: {}", a.summary()));
            }
            json!({"applied": true})
        }
        Err(e) => {
            ed.error(format!("{name} {label} refused, nothing was changed: {e}"));
            json!({"applied": false, "failureReason": e})
        }
    }
}
