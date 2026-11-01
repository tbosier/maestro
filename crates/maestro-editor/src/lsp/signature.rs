//! Signature help in insert mode: the signature of the call around the
//! cursor, with the parameter the cursor is in marked.
//!
//! A request goes out when a typed character is one of the server's
//! signature trigger characters (typically `(` and `,`), on `C-k`, and,
//! while help is showing or a request is out, after every edit or cursor
//! move, so the marked parameter follows typing. Each new request cancels
//! the one before. A reply is dropped unless the buffer version, the
//! cursors, the focused view and insert mode are what they were when it was
//! sent. An empty answer closes the popup, and so does leaving insert mode.

use maestro_core::{DocumentVersion, Selection};
use maestro_lsp::convert::char_position;
use maestro_lsp::signature::{self, Help};
use maestro_lsp::{Reply, RequestId, RpcError, Value, json};

use super::{REQUEST_TIMEOUT, ServerId, reconcile};
use crate::editor::{DocumentId, Editor, Mode, Severity, ViewId};
use crate::key::KeyEvent;

/// Why a request was sent. Maps to `SignatureHelpTriggerKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// `C-k`.
    Manual,
    Character(char),
    /// Help is showing and the text or cursor changed.
    Retrigger,
}

/// The popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Popup {
    pub doc: DocumentId,
    pub view: ViewId,
    pub help: Help,
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

/// Signature help state, part of [`super::Lsp`].
#[derive(Default)]
pub struct SignatureHelp {
    pub popup: Option<Popup>,
    /// The last answer as the server sent it, for `activeSignatureHelp`.
    raw: Option<Value>,
    pending: Option<Pending>,
    /// The version and cursors the popup or request is in step with.
    seen: Option<(DocumentVersion, Selection)>,
    next_token: u64,
    /// Why the last reply was not shown, for tests and diagnosis.
    pub last_dropped: Option<String>,
}

impl SignatureHelp {
    /// True while a request is out.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// The popup, if it belongs to the focused view in insert mode and the
/// text and cursors are what the last request was made for. A change
/// without a key (a mouse click, a paste) hides it until the next key
/// asks again.
pub fn visible(ed: &Editor) -> Option<&Popup> {
    let s = &ed.lsp.signature;
    let p = s.popup.as_ref()?;
    let in_step = s
        .seen
        .as_ref()
        .is_some_and(|(v, sel)| *v == ed.doc().version() && *sel == ed.view().selection);
    let ok = ed.mode == Mode::Insert
        && ed.prompt.is_none()
        && ed.picker.is_none()
        && ed.focus == p.view
        && ed.view().doc == p.doc
        && in_step;
    ok.then_some(p)
}

/// Shows `result` (protocol JSON) as the popup for the focused view, as a
/// reply would. For front-end tests. Returns false when it has no
/// signatures.
pub fn show(ed: &mut Editor, result: &Value) -> bool {
    let Some(help) = signature::parse(result) else {
        return false;
    };
    ed.lsp.signature.popup = Some(Popup {
        doc: ed.view().doc,
        view: ed.focus,
        help,
    });
    ed.lsp.signature.seen = Some((ed.doc().version(), ed.view().selection.clone()));
    true
}

/// Closes the popup and cancels any request behind it.
pub fn close(ed: &mut Editor) {
    let s = &mut ed.lsp.signature;
    s.popup = None;
    s.raw = None;
    s.seen = None;
    if let Some(p) = s.pending.take()
        && let Some(server) = ed.lsp.servers.get(&p.server)
    {
        server.client.cancel(p.id);
    }
}

/// Forgets requests to a server that stopped.
pub(super) fn server_stopped(ed: &mut Editor, server: ServerId) {
    let s = &mut ed.lsp.signature;
    if s.pending.as_ref().is_some_and(|p| p.server == server) {
        s.pending = None;
    }
    s.popup = None;
    s.seen = None;
}

fn active(ed: &Editor) -> Option<(DocumentId, ViewId)> {
    let s = &ed.lsp.signature;
    if let Some(p) = &s.pending {
        return Some((p.doc, p.view));
    }
    s.popup.as_ref().map(|p| (p.doc, p.view))
}

/// Hook after every key: closes the popup outside insert mode and starts
/// requests.
pub(crate) fn after_key(
    ed: &mut Editor,
    key: KeyEvent,
    mode_before: Mode,
    doc_before: DocumentId,
    version_before: DocumentVersion,
) {
    if ed.mode != Mode::Insert || ed.prompt.is_some() || ed.picker.is_some() {
        if active(ed).is_some() {
            close(ed);
        }
        return;
    }
    if ed.is_replaying() {
        return;
    }
    let mut on = active(ed);
    if on.is_some_and(|(doc, view)| doc != ed.view().doc || view != ed.focus) {
        close(ed);
        on = None;
    }
    let version = ed.doc().version();
    let head = ed.view().selection.primary().head;
    let typed = key.typed_char().filter(|&c| {
        mode_before == Mode::Insert
            && ed.view().doc == doc_before
            && version != version_before
            && head > 0
            && ed.doc().text().char(head - 1) == c
    });
    if let Some(c) = typed
        && trigger_characters(ed).contains(&c)
    {
        request(ed, Trigger::Character(c));
        return;
    }
    let moved = ed
        .lsp
        .signature
        .seen
        .as_ref()
        .is_none_or(|(v, sel)| *v != version || *sel != ed.view().selection);
    if on.is_some() && moved {
        request(ed, Trigger::Retrigger);
    }
}

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
    caps["signatureHelpProvider"]["triggerCharacters"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let mut chars = v.as_str()?.chars();
            let c = chars.next()?;
            chars.next().is_none().then_some(c)
        })
        .collect()
}

/// Asks for signature help at the focused view's primary cursor. Only a
/// manual request reports why nothing was sent.
pub fn request(ed: &mut Editor, trigger: Trigger) -> bool {
    let manual = trigger == Trigger::Manual;
    if ed.is_replaying() {
        return false;
    }
    let server = if manual {
        super::focused_server(ed, "signature help", "signatureHelpProvider")
    } else {
        reconcile(ed);
        let doc = ed.view().doc;
        ed.lsp.docs.get(&doc).and_then(|sync| {
            let s = ed.lsp.servers.get(&sync.server)?;
            s.supports("signatureHelpProvider")
                .then(|| (sync.server, sync.uri.clone()))
        })
    };
    let Some((server, uri)) = server else {
        return false;
    };
    let selection = ed.view().selection.clone();
    let mut context = json!({
        "triggerKind": match trigger {
            Trigger::Manual => 1,
            Trigger::Character(_) => 2,
            Trigger::Retrigger => 3,
        },
        "isRetrigger": ed.lsp.signature.popup.is_some(),
    });
    if let Trigger::Character(c) = trigger {
        context["triggerCharacter"] = json!(c.to_string());
    }
    if let Some(raw) = &ed.lsp.signature.raw {
        context["activeSignatureHelp"] = raw.clone();
    }
    let params = json!({
        "textDocument": {"uri": uri},
        "position": char_position(ed.doc().text(), selection.primary().head),
        "context": context,
    });
    if let Some(old) = ed.lsp.signature.pending.take()
        && let Some(s) = ed.lsp.servers.get(&old.server)
    {
        s.client.cancel(old.id);
    }
    let (id, rx) = ed.lsp.servers[&server]
        .client
        .request("textDocument/signatureHelp", params);
    let s = &mut ed.lsp.signature;
    s.next_token += 1;
    let token = s.next_token;
    let version = ed.buffers[&ed.views[&ed.focus].doc].doc.version();
    s.seen = Some((version, selection.clone()));
    s.pending = Some(Pending {
        token,
        server,
        id,
        doc: ed.views[&ed.focus].doc,
        view: ed.focus,
        version,
        selection,
        trigger,
    });
    let spawned = ed.spawn_background("lsp-signature", move |jobs| {
        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(reply) => {
                jobs.send(move |ed: &mut Editor| on_reply(ed, token, reply));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                jobs.send(move |ed: &mut Editor| {
                    if ed
                        .lsp
                        .signature
                        .pending
                        .as_ref()
                        .is_some_and(|p| p.token == token)
                    {
                        close(ed);
                    }
                });
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
        }
    });
    if let Err(e) = spawned {
        ed.lsp.signature.pending = None;
        ed.error(format!("signature help: {e}"));
        return false;
    }
    true
}

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
    let s = &mut ed.lsp.signature;
    if !s.pending.as_ref().is_some_and(|p| p.token == token) {
        return; // superseded
    }
    let p = s.pending.take().expect("checked");
    let manual = p.trigger == Trigger::Manual;
    if let Some(why) = stale_reason(ed, &p) {
        ed.lsp.signature.last_dropped = Some(why.to_string());
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
            ed.lsp.signature.last_dropped = Some("the server canceled it".into());
            return;
        }
        Err(e) => {
            ed.lsp.signature.last_dropped = Some(format!("error: {e}"));
            close(ed);
            if manual {
                ed.error(format!("signature help: {}", e.message));
            }
            return;
        }
    };
    ed.lsp.signature.last_dropped = None;
    if show(ed, &result) {
        ed.lsp.signature.raw = Some(result);
    } else {
        close(ed);
        if manual {
            ed.set_status(Severity::Info, "no signature help here");
        }
    }
}
