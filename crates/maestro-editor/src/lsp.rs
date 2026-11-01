//! Language servers: which server a buffer uses, document sync,
//! diagnostics, and applying replies.
//!
//! Everything here runs on the editor's thread. `maestro_lsp::Client` owns
//! the server process and its threads; its notifications reach
//! [`on_event`] through an idle [`JobSender`], and each request gets a
//! short-lived background thread that waits for the reply and sends it
//! back as a job (so the front end polls quickly while a request is out).
//!
//! The editor calls three hooks:
//!
//! * [`document_changed`] from every text-change path (`apply_in_view`,
//!   undo and redo, reload). It maps stored diagnostics through the change
//!   and sends an incremental `didChange`;
//! * [`document_saved`] after a buffer is written;
//! * [`reconcile`] after each key, on `open` and on `tick`. It opens and
//!   closes documents on servers to match the buffer list, starts servers
//!   as needed, and resends the full text of any document whose version
//!   moved without a `document_changed` call (a safety net, not the normal
//!   path).
//!
//! Completion in insert mode lives in [`completion`], signature help in
//! [`signature`], rename, formatting and code actions in [`actions`], and
//! applying the edits those (and `workspace/applyEdit`) produce in
//! [`edit`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use maestro_config::LanguageServerConfig;
use maestro_core::{Assoc, ChangeSet, DocumentVersion, Rope};
use maestro_lsp::convert::{self, path_to_uri, uri_to_path};
use maestro_lsp::{Client, Event, Reply, RequestId, RpcError, StartOptions, Value, json};

use crate::editor::{DocumentId, Editor, JobSender, Severity};
use crate::picker::{Picker, PickerAction, PickerItem};

pub mod actions;
pub mod completion;
pub mod edit;
pub mod signature;

/// How long a request may take before the editor gives up on it.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);

pub type ServerId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

impl DiagnosticSeverity {
    fn from_lsp(v: &Value) -> Self {
        match v.as_u64() {
            Some(2) => Self::Warning,
            Some(3) => Self::Info,
            Some(4) => Self::Hint,
            // The protocol leaves a missing severity to the client.
            _ => Self::Error,
        }
    }

    /// The letter drawn in the sign column, so severity never relies on
    /// color alone.
    pub fn letter(self) -> char {
        match self {
            Self::Error => 'E',
            Self::Warning => 'W',
            Self::Info => 'I',
            Self::Hint => 'H',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
            Self::Hint => "hint",
        }
    }
}

/// A diagnostic in char coordinates of the buffer's current text. Ranges
/// are mapped through every edit, so they stay attached to their text until
/// the server publishes again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub from: usize,
    pub to: usize,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: Option<String>,
    /// The diagnostic as the server published it (`Null` when made
    /// locally). Code action requests send it back, with its range
    /// updated, so the server sees its own code and data fields.
    pub raw: Value,
}

/// Hover text shown in a popup until the next key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverPopup {
    pub doc: DocumentId,
    /// Char position the hover was requested at.
    pub anchor: usize,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestKind {
    Definition,
    References,
    Hover,
}

impl RequestKind {
    fn label(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::References => "references",
            Self::Hover => "hover",
        }
    }

    fn method(self) -> &'static str {
        match self {
            Self::Definition => "textDocument/definition",
            Self::References => "textDocument/references",
            Self::Hover => "textDocument/hover",
        }
    }

    fn capability(self) -> &'static str {
        match self {
            Self::Definition => "definitionProvider",
            Self::References => "referencesProvider",
            Self::Hover => "hoverProvider",
        }
    }
}

struct Server {
    name: String,
    command: String,
    args: Vec<String>,
    root: PathBuf,
    client: Client,
    capabilities: Option<Value>,
}

impl Server {
    fn ready(&self) -> bool {
        self.capabilities.is_some()
    }

    /// `TextDocumentSyncKind`: 0 none, 1 full, 2 incremental.
    fn sync_kind(&self) -> u64 {
        let sync = self
            .capabilities
            .as_ref()
            .and_then(|c| c.get("textDocumentSync"));
        match sync {
            Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
            Some(Value::Object(o)) => o.get("change").and_then(Value::as_u64).unwrap_or(0),
            _ => 0,
        }
    }

    fn wants_save(&self) -> bool {
        let sync = self
            .capabilities
            .as_ref()
            .and_then(|c| c.get("textDocumentSync"));
        match sync.and_then(|s| s.get("save")) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Object(_)) => true,
            _ => false,
        }
    }

    fn wants_save_text(&self) -> bool {
        self.capabilities
            .as_ref()
            .and_then(|c| c["textDocumentSync"]["save"]["includeText"].as_bool())
            .unwrap_or(false)
    }

    fn supports(&self, capability: &str) -> bool {
        match self.capabilities.as_ref().and_then(|c| c.get(capability)) {
            None | Some(Value::Null) | Some(Value::Bool(false)) => false,
            Some(_) => true,
        }
    }
}

/// What the server has been told about a buffer.
struct SyncedDoc {
    server: ServerId,
    uri: String,
    path: PathBuf,
    /// The text as the server has it, used for the old-text positions of
    /// the next incremental change.
    text: Rope,
    version: DocumentVersion,
}

struct PendingRequest {
    token: u64,
    server: ServerId,
    id: RequestId,
    doc: DocumentId,
    version: DocumentVersion,
    /// Every buffer's version when the request was sent. Locations in the
    /// reply may point into any of them.
    versions: HashMap<DocumentId, DocumentVersion>,
    /// The cursor the request was made at.
    head: usize,
}

/// Files read for a reply, keyed by path. Only regular files up to
/// [`MAX_PRELOAD_BYTES`] each, at most [`MAX_PRELOAD_FILES`] of them.
type Preloaded = HashMap<PathBuf, Rope>;
const MAX_PRELOAD_FILES: usize = 500;
const MAX_PRELOAD_BYTES: u64 = 16 * 1024 * 1024;

/// Reads the files a list of locations points into. Runs on a background
/// thread. Special files (FIFOs, devices) are never opened.
fn preload_files(locations: &[(String, Value)]) -> Preloaded {
    let mut out = Preloaded::new();
    for (uri, _) in locations {
        if out.len() >= MAX_PRELOAD_FILES {
            break;
        }
        let Some(path) = uri_to_path(uri) else {
            continue;
        };
        if out.contains_key(&path) {
            continue;
        }
        let regular =
            std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() <= MAX_PRELOAD_BYTES);
        if regular && let Ok(loaded) = maestro_core::io::load(&path) {
            out.insert(path, loaded.text);
        }
    }
    out
}

/// Language server state, one per editor.
#[derive(Default)]
pub struct Lsp {
    /// Off by default so that editors built in tests or for headless use
    /// never start processes on their own; the terminal front end turns it
    /// on.
    pub enabled: bool,
    servers: BTreeMap<ServerId, Server>,
    docs: HashMap<DocumentId, SyncedDoc>,
    diagnostics: HashMap<DocumentId, Vec<Diagnostic>>,
    pending: HashMap<RequestKind, PendingRequest>,
    /// Languages whose servers are not installed; reported once.
    missing: HashSet<String>,
    /// (command, root) pairs that crashed or failed to start. They stay
    /// stopped until `:lsp-restart`, so a broken server cannot loop.
    stopped: HashSet<(String, PathBuf)>,
    pub hover: Option<HoverPopup>,
    /// The completion popup and its request.
    pub completion: completion::Completion,
    /// The signature help popup and its request.
    pub signature: signature::SignatureHelp,
    /// Rename, formatting and code action requests.
    pub actions: actions::Actions,
    /// The log of each server that stopped, kept so `:lsp-log` can show
    /// why. One entry per server name, the latest stop.
    stopped_logs: BTreeMap<String, Vec<String>>,
    next_id: u64,
}

impl Lsp {
    fn fresh_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Diagnostics for a buffer, sorted by start.
    pub fn diagnostics(&self, doc: DocumentId) -> &[Diagnostic] {
        self.diagnostics.get(&doc).map_or(&[], Vec::as_slice)
    }

    /// Replaces a buffer's diagnostics. Used by tests and front ends that
    /// render without a server; normal updates come from the server.
    pub fn set_diagnostics(&mut self, doc: DocumentId, mut list: Vec<Diagnostic>) {
        list.sort_by_key(|d| (d.from, d.severity));
        self.diagnostics.insert(doc, list);
    }

    /// Names of running servers, for display.
    pub fn server_names(&self) -> Vec<String> {
        self.servers.values().map(|s| s.name.clone()).collect()
    }

    /// True if `doc` is open on a server.
    pub fn is_synced(&self, doc: DocumentId) -> bool {
        self.docs.contains_key(&doc)
    }

    /// Sends a request about `doc` and returns the reply channel. For tests
    /// and diagnostics tooling; editor features use typed requests.
    pub fn raw_request(
        &self,
        doc: DocumentId,
        method: &str,
        params: Value,
    ) -> Option<std::sync::mpsc::Receiver<Reply>> {
        let sync = self.docs.get(&doc)?;
        let server = self.servers.get(&sync.server)?;
        let mut params = params;
        params["textDocument"] = json!({"uri": sync.uri});
        Some(server.client.request(method, params).1)
    }
}

// ----- languages and roots ---------------------------------------------------

/// The config key and protocol language id for a file, by extension.
pub fn language_for(path: &Path) -> Option<(&'static str, &'static str)> {
    let ext = path.extension()?.to_str()?;
    Some(match ext {
        "rs" => ("rust", "rust"),
        "py" | "pyi" => ("python", "python"),
        "ts" | "mts" | "cts" => ("typescript", "typescript"),
        "tsx" => ("typescript", "typescriptreact"),
        "js" | "mjs" | "cjs" => ("javascript", "javascript"),
        "jsx" => ("javascript", "javascriptreact"),
        "c" | "h" => ("c", "c"),
        "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" => ("cpp", "cpp"),
        _ => return None,
    })
}

/// The workspace root for a file: the nearest ancestor with `.git`, else
/// the nearest with a project file for the language, else its directory.
pub fn find_root(path: &Path, language: &str) -> PathBuf {
    let dir = path.parent().unwrap_or(Path::new("/"));
    if let Some(a) = dir.ancestors().find(|a| a.join(".git").exists()) {
        return a.to_path_buf();
    }
    let markers: &[&str] = match language {
        "rust" => &["Cargo.toml"],
        "python" => &["pyproject.toml", "setup.py", "setup.cfg"],
        "typescript" | "javascript" => &["package.json", "tsconfig.json", "jsconfig.json"],
        "c" | "cpp" => &["compile_commands.json", "CMakeLists.txt"],
        _ => &[],
    };
    dir.ancestors()
        .find(|a| markers.iter().any(|m| a.join(m).exists()))
        .unwrap_or(dir)
        .to_path_buf()
}

// ----- hooks -----------------------------------------------------------------

/// Hook for every text change. `changes` are the change sets applied to
/// `doc`, in order, since the last call.
pub fn document_changed(ed: &mut Editor, doc: DocumentId, changes: &[ChangeSet]) {
    if let Some(list) = ed.lsp.diagnostics.get_mut(&doc) {
        for cs in changes {
            for d in list.iter_mut() {
                d.from = cs.map_pos(d.from, Assoc::After);
                d.to = cs.map_pos(d.to, Assoc::Before).max(d.from);
            }
        }
    }
    if ed.lsp.hover.as_ref().is_some_and(|h| h.doc == doc) {
        ed.lsp.hover = None;
    }
    completion::document_changed(ed, doc, changes);
    let Some(buf) = ed.buffers.get(&doc) else {
        return;
    };
    let Lsp { docs, servers, .. } = &mut ed.lsp;
    let Some(sync) = docs.get_mut(&doc) else {
        return;
    };
    let Some(server) = servers.get(&sync.server) else {
        return;
    };
    // An empty change set leaves the text and the document version alone
    // (see `Document::apply`), so it is not a change to report.
    let changes: Vec<&ChangeSet> = changes.iter().filter(|c| !c.is_empty()).collect();
    let version = buf.doc.version();
    if changes.is_empty() && version == sync.version {
        return;
    }
    let kind = server.sync_kind();
    let mut content = Vec::new();
    let mut lost_track = false;
    for cs in &changes {
        if cs.len() != sync.text.len_chars() {
            lost_track = true;
            break;
        }
        if kind == 2 {
            // Positions come from the text before this change set.
            content.extend(convert::content_changes(&sync.text, cs));
        }
        if cs.apply(&mut sync.text).is_err() {
            lost_track = true;
            break;
        }
    }
    let expected = sync.version.0 + changes.len() as u64;
    if lost_track || version.0 != expected || kind == 1 {
        sync.text = buf.doc.text().clone();
        content = vec![json!({"text": sync.text.to_string()})];
    }
    sync.version = version;
    if kind == 0 {
        return;
    }
    server.client.notify(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": sync.uri, "version": version.0},
            "contentChanges": content,
        }),
    );
}

/// Hook after a buffer is written to disk.
pub fn document_saved(ed: &mut Editor, doc: DocumentId) {
    let Some(sync) = ed.lsp.docs.get(&doc) else {
        return;
    };
    if let Some(server) = ed.lsp.servers.get(&sync.server)
        && server.wants_save()
    {
        let mut params = json!({"textDocument": {"uri": sync.uri}});
        if server.wants_save_text()
            && let Some(buf) = ed.buffers.get(&doc)
        {
            params["text"] = json!(buf.doc.text().to_string());
        }
        server.client.notify("textDocument/didSave", params);
    }
}

/// Hook at the start of each key: the hover popup closes and a pending
/// completion debounce is superseded.
pub fn on_key(ed: &mut Editor) {
    ed.lsp.hover = None;
    completion::on_key(ed);
}

/// Matches open documents on servers to the buffer list. Cheap when
/// nothing changed: one pass over buffers and synced documents.
pub fn reconcile(ed: &mut Editor) {
    if !ed.lsp.enabled {
        return;
    }
    let gone: Vec<DocumentId> = ed
        .lsp
        .docs
        .iter()
        .filter(|(id, s)| {
            ed.buffers
                .get(id)
                .is_none_or(|b| b.doc.path() != Some(s.path.as_path()))
        })
        .map(|(id, _)| *id)
        .collect();
    for id in gone {
        close_doc(ed, id);
    }
    let unsynced: Vec<(DocumentId, PathBuf)> = ed
        .buffers
        .values()
        .filter(|b| !ed.lsp.docs.contains_key(&b.id))
        .filter_map(|b| Some((b.id, b.doc.path()?.to_path_buf())))
        .collect();
    for (id, path) in unsynced {
        let Some((language, language_id)) = language_for(&path) else {
            continue;
        };
        let Some(server) = server_for(ed, language, &path) else {
            continue;
        };
        if ed.lsp.servers.get(&server).is_some_and(Server::ready) {
            open_doc(ed, id, server, path, language_id);
        }
    }
    // Safety net: a change path that skipped `document_changed`.
    let drifted: Vec<DocumentId> = ed
        .lsp
        .docs
        .iter()
        .filter(|(id, s)| {
            ed.buffers
                .get(id)
                .is_some_and(|b| b.doc.version() != s.version)
        })
        .map(|(id, _)| *id)
        .collect();
    for id in drifted {
        document_changed(ed, id, &[]);
    }
}

fn open_doc(ed: &mut Editor, id: DocumentId, server: ServerId, path: PathBuf, language_id: &str) {
    let Some(buf) = ed.buffers.get(&id) else {
        return;
    };
    let text = buf.doc.text().clone();
    let version = buf.doc.version();
    let uri = path_to_uri(&path);
    if let Some(s) = ed.lsp.servers.get(&server) {
        s.client.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri,
                "languageId": language_id,
                "version": version.0,
                "text": text.to_string(),
            }}),
        );
    }
    ed.lsp.docs.insert(
        id,
        SyncedDoc {
            server,
            uri,
            path,
            text,
            version,
        },
    );
}

fn close_doc(ed: &mut Editor, id: DocumentId) {
    ed.lsp.diagnostics.remove(&id);
    let Some(sync) = ed.lsp.docs.remove(&id) else {
        return;
    };
    if let Some(s) = ed.lsp.servers.get(&sync.server) {
        s.client.notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": sync.uri}}),
        );
    }
}

/// The server for a file, starting one if needed. `None` when the
/// language has no usable server.
fn server_for(ed: &mut Editor, language: &str, path: &Path) -> Option<ServerId> {
    if ed.lsp.missing.contains(language) {
        return None;
    }
    let candidates = ed.config.language_servers(language);
    if candidates.is_empty() {
        return None;
    }
    let root = find_root(path, language);
    for c in &candidates {
        if let Some((id, _)) = ed
            .lsp
            .servers
            .iter()
            .find(|(_, s)| s.command == c.command && s.args == c.args && s.root == root)
        {
            return Some(*id);
        }
        if ed.lsp.stopped.contains(&(c.command.clone(), root.clone())) {
            return None;
        }
    }
    let path_var = std::env::var_os("PATH");
    let found = candidates.iter().find_map(|c| {
        maestro_lsp::find_executable(&c.command, path_var.as_deref()).map(|p| (c.clone(), p))
    });
    let Some((config, program)) = found else {
        ed.lsp.missing.insert(language.to_string());
        let names: Vec<&str> = candidates.iter().map(|c| c.command.as_str()).collect();
        ed.set_status(
            Severity::Warning,
            format!(
                "no language server for {language}: {} not found on PATH",
                names.join(" or ")
            ),
        );
        return None;
    };
    start_server(ed, &config, program, root)
}

fn start_server(
    ed: &mut Editor,
    config: &LanguageServerConfig,
    program: PathBuf,
    root: PathBuf,
) -> Option<ServerId> {
    let id = ed.lsp.fresh_id();
    let name = Path::new(&config.command)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| config.command.clone());
    let events: JobSender = ed.idle_job_sender();
    let sink = Box::new(move |event: Event| {
        // Only queue work for what the editor uses, so a chatty server
        // does not fill the job queue with notifications nobody reads.
        let wanted = match &event {
            // Requests must always be answered.
            Event::Exited { .. } | Event::Request { .. } => true,
            Event::Notification { method, .. } => {
                matches!(
                    method.as_str(),
                    "textDocument/publishDiagnostics" | "window/showMessage"
                )
            }
        };
        if wanted {
            events.send(move |ed: &mut Editor| on_event(ed, id, event));
        }
    });
    let opts = StartOptions {
        program,
        args: config.args.clone(),
        root: root.clone(),
    };
    let (client, init) = match Client::start(opts, sink) {
        Ok(started) => started,
        Err(e) => {
            ed.lsp.stopped.insert((config.command.clone(), root));
            ed.error(format!("could not start {name}: {e}"));
            return None;
        }
    };
    ed.lsp.servers.insert(
        id,
        Server {
            name,
            command: config.command.clone(),
            args: config.args.clone(),
            root,
            client,
            capabilities: None,
        },
    );
    let spawned = ed.spawn_background("lsp-init", move |jobs| {
        let reply = maestro_lsp::client::recv_reply(&init, INITIALIZE_TIMEOUT);
        jobs.send(move |ed: &mut Editor| on_initialized(ed, id, reply));
    });
    if let Err(e) = spawned {
        stop_server(ed, id, &format!("could not wait for it: {e}"));
        return None;
    }
    Some(id)
}

fn on_initialized(ed: &mut Editor, id: ServerId, reply: Option<Reply>) {
    let Some(server) = ed.lsp.servers.get_mut(&id) else {
        return;
    };
    match reply {
        Some(Ok(result)) => {
            server.capabilities = Some(result.get("capabilities").cloned().unwrap_or(json!({})));
            reconcile(ed);
        }
        Some(Err(e)) => stop_server(ed, id, &format!("initialize failed: {e}")),
        None if server.client.is_alive() => {
            stop_server(ed, id, "no answer to initialize within 60 s")
        }
        // The exit event reports a dead server.
        None => {}
    }
}

/// Removes a server, keeps it from restarting on its own, and tells the
/// user. Its documents lose their diagnostics.
fn stop_server(ed: &mut Editor, id: ServerId, why: &str) {
    let Some(server) = ed.lsp.servers.remove(&id) else {
        return;
    };
    ed.lsp
        .stopped
        .insert((server.command.clone(), server.root.clone()));
    let docs: Vec<DocumentId> = ed
        .lsp
        .docs
        .iter()
        .filter(|(_, s)| s.server == id)
        .map(|(d, _)| *d)
        .collect();
    for d in docs {
        ed.lsp.docs.remove(&d);
        ed.lsp.diagnostics.remove(&d);
    }
    ed.lsp.pending.retain(|_, p| p.server != id);
    completion::server_stopped(ed, id);
    signature::server_stopped(ed, id);
    actions::server_stopped(ed, id);
    ed.set_status(
        Severity::Warning,
        format!(
            "{} stopped: {why}; :lsp-restart starts it again",
            server.name
        ),
    );
    ed.lsp
        .stopped_logs
        .insert(server.name.clone(), server.client.log());
    shut_down_in_background(server);
}

/// Dropping a client waits briefly for the process; do that off the
/// editor thread.
fn shut_down_in_background(server: Server) {
    let _ = std::thread::Builder::new()
        .name("lsp-shutdown".into())
        .spawn(move || drop(server));
}

/// Applies something a server sent.
pub fn on_event(ed: &mut Editor, id: ServerId, event: Event) {
    if !ed.lsp.servers.contains_key(&id) {
        return;
    }
    match event {
        Event::Exited { status } => stop_server(ed, id, &format!("it exited ({status})")),
        Event::Request {
            id: request,
            method,
            params,
        } => {
            let answer = match method.as_str() {
                "workspace/applyEdit" => Ok(actions::apply_edit_request(ed, id, &params)),
                _ => Err(RpcError {
                    code: RpcError::METHOD_NOT_FOUND,
                    message: format!("maestro does not handle {method}"),
                }),
            };
            if let Some(s) = ed.lsp.servers.get(&id) {
                s.client.respond(request, answer);
            }
        }
        Event::Notification { method, params } => match method.as_str() {
            "textDocument/publishDiagnostics" => publish_diagnostics(ed, id, &params),
            "window/showMessage" => {
                let msg = params["message"].as_str().unwrap_or_default();
                let name = &ed.lsp.servers[&id].name;
                let text = format!("{name}: {msg}");
                match params["type"].as_u64() {
                    Some(1) => ed.error(text),
                    Some(2) => ed.set_status(Severity::Warning, text),
                    _ => {}
                }
            }
            _ => {}
        },
    }
}

fn publish_diagnostics(ed: &mut Editor, server: ServerId, params: &Value) {
    let Some(uri) = params["uri"].as_str() else {
        return;
    };
    let Some((&doc, sync)) = ed
        .lsp
        .docs
        .iter()
        .find(|(_, s)| s.server == server && s.uri == uri)
    else {
        // Diagnostics for files that are not open are not shown.
        return;
    };
    let Some(buf) = ed.buffers.get(&doc) else {
        return;
    };
    // Versioned diagnostics for an older text would land on the wrong
    // characters; the server publishes again for the current version.
    if let Some(v) = params["version"].as_u64()
        && (v != sync.version.0 || sync.version != buf.doc.version())
    {
        return;
    }
    let text = buf.doc.text();
    let list: Vec<Diagnostic> = params["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| {
            let (from, to) = convert::range_to_chars(text, &d["range"])?;
            Some(Diagnostic {
                from,
                to,
                severity: DiagnosticSeverity::from_lsp(&d["severity"]),
                message: d["message"].as_str().unwrap_or_default().to_string(),
                source: d["source"].as_str().map(str::to_string),
                raw: d.clone(),
            })
        })
        .collect();
    ed.lsp.set_diagnostics(doc, list);
}

// ----- requests ---------------------------------------------------------------

/// The focused buffer's server and the buffer's URI, if the server is
/// running and offers `capability`. Otherwise says why in the status line,
/// starting with `what`.
pub(crate) fn focused_server(
    ed: &mut Editor,
    what: &str,
    capability: &str,
) -> Option<(ServerId, String)> {
    reconcile(ed);
    let doc = ed.view().doc;
    let Some(sync) = ed.lsp.docs.get(&doc) else {
        let why = match ed.doc().path() {
            None => "this buffer has no file",
            Some(p) if language_for(p).is_none() => "no language server for this file type",
            Some(_) if !ed.lsp.enabled => "language servers are off",
            Some(_) => "the language server is not running or still starting",
        };
        ed.set_status(Severity::Warning, format!("{what}: {why}"));
        return None;
    };
    let server = ed.lsp.servers.get(&sync.server)?;
    if !server.supports(capability) {
        let msg = format!("{} does not support {what}", server.name);
        ed.set_status(Severity::Warning, msg);
        return None;
    }
    Some((sync.server, sync.uri.clone()))
}

/// Sends a position request for the focused view's cursor. Returns false
/// (with a status message) when no server can answer.
pub fn request_at_cursor(ed: &mut Editor, kind: RequestKind) -> bool {
    reconcile(ed);
    let doc = ed.view().doc;
    let head = ed.view().selection.primary().head;
    let Some(sync) = ed.lsp.docs.get(&doc) else {
        let why = match ed.doc().path() {
            None => "this buffer has no file".to_string(),
            Some(p) if language_for(p).is_none() => "no language server for this file type".into(),
            Some(_) if !ed.lsp.enabled => "language servers are off".into(),
            Some(_) => "the language server is not running or still starting".into(),
        };
        ed.set_status(Severity::Warning, format!("{}: {why}", kind.label()));
        return false;
    };
    let Some(server) = ed.lsp.servers.get(&sync.server) else {
        return false;
    };
    if !server.supports(kind.capability()) {
        let msg = format!("{} does not support {}", server.name, kind.label());
        ed.set_status(Severity::Warning, msg);
        return false;
    }
    let mut params = json!({
        "textDocument": {"uri": sync.uri},
        "position": convert::char_position(ed.doc().text(), head),
    });
    if kind == RequestKind::References {
        params["context"] = json!({"includeDeclaration": true});
    }
    let server_id = sync.server;
    let version = ed.doc().version();
    if let Some(old) = ed.lsp.pending.remove(&kind)
        && let Some(s) = ed.lsp.servers.get(&old.server)
    {
        s.client.cancel(old.id);
    }
    let (id, rx) = ed.lsp.servers[&server_id]
        .client
        .request(kind.method(), params);
    let token = ed.lsp.fresh_id();
    let versions = ed
        .buffers
        .iter()
        .map(|(id, b)| (*id, b.doc.version()))
        .collect();
    ed.lsp.pending.insert(
        kind,
        PendingRequest {
            token,
            server: server_id,
            id,
            doc,
            version,
            versions,
            head,
        },
    );
    let spawned = ed.spawn_background("lsp-request", move |jobs| {
        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(reply) => {
                // Reading the files that locations point into happens here,
                // off the editor thread.
                let files = match (&reply, kind) {
                    (Ok(v), RequestKind::Definition | RequestKind::References) => {
                        preload_files(&convert::locations(v))
                    }
                    _ => HashMap::new(),
                };
                jobs.send(move |ed: &mut Editor| on_reply(ed, kind, token, reply, files));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                jobs.send(move |ed: &mut Editor| on_timeout(ed, kind, token));
            }
            // Cancelled, superseded or the server died.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
        }
    });
    if let Err(e) = spawned {
        ed.lsp.pending.remove(&kind);
        ed.error(format!("{}: {e}", kind.label()));
        return false;
    }
    true
}

fn on_timeout(ed: &mut Editor, kind: RequestKind, token: u64) {
    if ed.lsp.pending.get(&kind).is_some_and(|p| p.token == token) {
        let p = ed.lsp.pending.remove(&kind).expect("checked");
        if let Some(s) = ed.lsp.servers.get(&p.server) {
            s.client.cancel(p.id);
        }
        ed.set_status(
            Severity::Warning,
            format!(
                "{}: no answer within {} s",
                kind.label(),
                REQUEST_TIMEOUT.as_secs()
            ),
        );
    }
}

/// Applies a reply on the main thread. A reply is dropped if any open
/// buffer changed since the request was sent: its positions may describe
/// text that no longer exists, in the requesting buffer or in the one a
/// location points into. Edits only made between request and reply, so
/// this rarely costs a real answer.
fn on_reply(ed: &mut Editor, kind: RequestKind, token: u64, reply: Reply, files: Preloaded) {
    if !ed.lsp.pending.get(&kind).is_some_and(|p| p.token == token) {
        return; // superseded
    }
    let p = ed.lsp.pending.remove(&kind).expect("checked");
    let current = ed.buffers.get(&p.doc).map(|b| b.doc.version());
    if current != Some(p.version) {
        ed.info(format!(
            "{} result dropped: the buffer changed while waiting",
            kind.label()
        ));
        return;
    }
    let other_changed = p
        .versions
        .iter()
        .any(|(id, v)| ed.buffers.get(id).is_some_and(|b| b.doc.version() != *v));
    if other_changed {
        ed.info(format!(
            "{} result dropped: another buffer changed while waiting",
            kind.label()
        ));
        return;
    }
    if ed.view().doc != p.doc {
        ed.info(format!(
            "{} result dropped: another buffer is shown now",
            kind.label()
        ));
        return;
    }
    // The user moved on to something else (a prompt, a picker, typing):
    // jumping or opening a picker now would act on input meant for that.
    if ed.prompt.is_some() || ed.picker.is_some() || ed.mode == crate::editor::Mode::Insert {
        ed.info(format!(
            "{} result dropped: arrived while you were typing or in a picker",
            kind.label()
        ));
        return;
    }
    if kind == RequestKind::Hover && ed.view().selection.primary().head != p.head {
        ed.info("hover result dropped: the cursor moved");
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
            ed.info(format!("{}: the server canceled the request", kind.label()));
            return;
        }
        Err(e) => {
            ed.error(format!("{}: {e}", kind.label()));
            return;
        }
    };
    match kind {
        RequestKind::Definition => show_definition(ed, &result, &files),
        RequestKind::References => show_references(ed, &result, &files),
        RequestKind::Hover => show_hover(ed, &result, p.doc, p.head),
    }
}

fn show_definition(ed: &mut Editor, result: &Value, files: &Preloaded) {
    let locations = convert::locations(result);
    match locations.as_slice() {
        [] => ed.info("no definition found"),
        [(uri, range)] => goto_location(ed, uri, range, files),
        _ => {
            let items = location_items(ed, &locations, files);
            ed.picker = Some(Picker::new("definitions", items));
        }
    }
}

fn show_references(ed: &mut Editor, result: &Value, files: &Preloaded) {
    let locations = convert::locations(result);
    if locations.is_empty() {
        ed.info("no references found");
        return;
    }
    let items = location_items(ed, &locations, files);
    ed.picker = Some(Picker::new("references", items));
}

fn show_hover(ed: &mut Editor, result: &Value, doc: DocumentId, anchor: usize) {
    match convert::hover_text(result) {
        Some(text) => {
            ed.lsp.hover = Some(HoverPopup {
                doc,
                anchor,
                lines: text.lines().map(str::to_string).collect(),
            });
        }
        None => ed.info("no hover information"),
    }
}

/// The text of `path`: the open buffer's if there is one, else the copy
/// read in the background. Never reads files on the editor thread.
fn text_for(ed: &Editor, path: &Path, files: &Preloaded) -> Option<Rope> {
    match ed.find_buffer_by_path(path) {
        Some(id) => Some(ed.buffers[&id].doc.text().clone()),
        None => files.get(path).cloned(),
    }
}

fn goto_location(ed: &mut Editor, uri: &str, range: &Value, files: &Preloaded) {
    let Some(path) = uri_to_path(uri) else {
        ed.error(format!("cannot open {uri}"));
        return;
    };
    // Only open what is already a buffer or was read as a regular file in
    // the background, so a location cannot make the editor block on a FIFO.
    if ed.find_buffer_by_path(&path).is_none() && !files.contains_key(&path) {
        ed.error(format!("cannot open {}", path.display()));
        return;
    }
    // Re-check right before the synchronous open: the background read saw a
    // regular file, but it could have been replaced since.
    if ed.find_buffer_by_path(&path).is_none()
        && !std::fs::metadata(&path).is_ok_and(|m| m.is_file())
    {
        ed.error(format!(
            "cannot open {}: not a regular file",
            path.display()
        ));
        return;
    }
    ed.push_jump();
    if let Err(e) = ed.open(&path) {
        ed.error(e);
        return;
    }
    let text = ed.doc().text().clone();
    let Some((from, _)) = convert::range_to_chars(&text, range) else {
        ed.error("the server pointed past the end of the file");
        return;
    };
    ed.set_selection(maestro_core::Selection::point(from));
    crate::commands::align_view_center(ed);
}

/// Picker items for locations, with line text, in the order given.
fn location_items(
    ed: &Editor,
    locations: &[(String, Value)],
    files: &Preloaded,
) -> Vec<PickerItem> {
    let mut texts: HashMap<PathBuf, Option<Rope>> = HashMap::new();
    locations
        .iter()
        .filter_map(|(uri, range)| {
            let path = uri_to_path(uri)?;
            let text = texts
                .entry(path.clone())
                .or_insert_with(|| text_for(ed, &path, files))
                .as_ref()?;
            let (from, _) = convert::range_to_chars(text, range)?;
            let line = text.char_to_line(from);
            let column = from - text.line_to_char(line);
            let line_text = text.line(line).to_string();
            Some(PickerItem {
                label: format!(
                    "{}:{}:{}  {}",
                    display_path(ed, &path),
                    line + 1,
                    column + 1,
                    line_text.trim()
                ),
                detail: String::new(),
                action: PickerAction::Goto { path, line, column },
            })
        })
        .collect()
}

fn display_path(ed: &Editor, path: &Path) -> String {
    path.strip_prefix(&ed.cwd)
        .unwrap_or(path)
        .display()
        .to_string()
}

// ----- diagnostics for the interface ----------------------------------------

/// The most severe diagnostic starting on each line in `lines`, for the
/// sign column.
pub fn line_signs(
    ed: &Editor,
    doc: DocumentId,
    lines: std::ops::Range<usize>,
) -> HashMap<usize, DiagnosticSeverity> {
    let mut out: HashMap<usize, DiagnosticSeverity> = HashMap::new();
    let Some(buf) = ed.buffers.get(&doc) else {
        return out;
    };
    let text = buf.doc.text();
    let first = text.line_to_char(lines.start.min(text.len_lines()));
    let last = text.line_to_char(lines.end.min(text.len_lines()));
    let list = ed.lsp.diagnostics(doc);
    let start = list.partition_point(|d| d.from < first);
    for d in list[start..].iter().take_while(|d| d.from < last) {
        let line = text.char_to_line(d.from.min(text.len_chars()));
        let e = out.entry(line).or_insert(d.severity);
        *e = (*e).min(d.severity);
    }
    out
}

/// Diagnostics overlapping chars `from..to`, as (from, to, severity) with
/// empty ranges widened to one char so they can be seen.
pub fn spans_in(
    ed: &Editor,
    doc: DocumentId,
    from: usize,
    to: usize,
) -> Vec<(usize, usize, DiagnosticSeverity)> {
    ed.lsp
        .diagnostics(doc)
        .iter()
        .map(|d| (d.from, d.to.max(d.from + 1), d.severity))
        .filter(|&(a, b, _)| a < to && b > from)
        .collect()
}

/// The diagnostic to show for the focused cursor's line: the most severe,
/// then the first.
pub fn cursor_diagnostic(ed: &Editor) -> Option<&Diagnostic> {
    let doc = ed.view().doc;
    let text = ed.doc().text();
    let head = ed.view().selection.primary().head.min(text.len_chars());
    let line = text.char_to_line(head);
    let start = text.line_to_char(line);
    let end = text.line_to_char((line + 1).min(text.len_lines()));
    ed.lsp
        .diagnostics(doc)
        .iter()
        .filter(|d| {
            (d.from >= start && d.from < end.max(start + 1)) || (d.from < start && d.to > start)
        })
        .min_by_key(|d| d.severity)
}

// ----- commands -------------------------------------------------------------

/// Moves to the next (or previous) diagnostic in the buffer, wrapping, and
/// selects its range.
pub fn goto_diagnostic(ed: &mut Editor, forward: bool) {
    let doc = ed.view().doc;
    let at = ed.view().selection.primary().from();
    let list = ed.lsp.diagnostics(doc);
    if list.is_empty() {
        ed.info("no diagnostics in this buffer");
        return;
    }
    let target = if forward {
        list.iter().find(|d| d.from > at).or(list.first())
    } else {
        list.iter().rev().find(|d| d.from < at).or(list.last())
    };
    let Some(d) = target.cloned() else {
        return;
    };
    ed.push_jump();
    let last = d.to.saturating_sub(1).max(d.from);
    ed.set_selection(maestro_core::Selection::single(d.from, last));
    ed.set_status(
        Severity::Info,
        format!("{}: {}", d.severity.label(), d.message),
    );
}

/// A picker of diagnostics: the focused buffer's, or every buffer's.
pub fn diagnostics_picker(ed: &mut Editor, all: bool) {
    let focused = ed.view().doc;
    let mut items = Vec::new();
    for (id, buf) in &ed.buffers {
        if !all && *id != focused {
            continue;
        }
        let Some(path) = buf.doc.path() else {
            continue;
        };
        let text = buf.doc.text();
        for d in ed.lsp.diagnostics(*id) {
            let from = d.from.min(text.len_chars());
            let line = text.char_to_line(from);
            let column = from - text.line_to_char(line);
            let message = d.message.lines().next().unwrap_or_default();
            let place = if all {
                format!("{}:{}:{}", display_path(ed, path), line + 1, column + 1)
            } else {
                format!("{}:{}", line + 1, column + 1)
            };
            items.push(PickerItem {
                label: format!("{} {place}  {message}", d.severity.letter()),
                detail: d.source.clone().unwrap_or_default(),
                action: PickerAction::Goto {
                    path: path.to_path_buf(),
                    line,
                    column,
                },
            });
        }
    }
    if items.is_empty() {
        ed.info(if all {
            "no diagnostics in open buffers"
        } else {
            "no diagnostics in this buffer"
        });
        return;
    }
    let title = if all {
        "diagnostics: all buffers"
    } else {
        "diagnostics"
    };
    ed.picker = Some(Picker::new(title, items));
}

/// Stops every server and starts fresh ones for the open buffers.
pub fn restart(ed: &mut Editor) {
    let servers = std::mem::take(&mut ed.lsp.servers);
    for (_, s) in servers {
        shut_down_in_background(s);
    }
    ed.lsp.docs.clear();
    ed.lsp.diagnostics.clear();
    ed.lsp.pending.clear();
    ed.lsp.hover = None;
    completion::close(ed);
    signature::close(ed);
    actions::reset(ed);
    ed.lsp.missing.clear();
    ed.lsp.stopped.clear();
    if !ed.lsp.enabled {
        ed.set_status(
            Severity::Warning,
            "language servers are off in this session",
        );
        return;
    }
    ed.info("restarting language servers");
    reconcile(ed);
}

/// Opens a scratch buffer with each server's captured stderr and log
/// messages, including servers that stopped.
pub fn show_log(ed: &mut Editor) {
    if ed.lsp.servers.is_empty() && ed.lsp.stopped_logs.is_empty() {
        ed.info("no language servers have run in this session");
        return;
    }
    let mut text = String::new();
    let running = ed
        .lsp
        .servers
        .values()
        .map(|s| (format!("{} ({})", s.name, s.root.display()), s.client.log()));
    let stopped = ed
        .lsp
        .stopped_logs
        .iter()
        .map(|(name, log)| (format!("{name} (stopped)"), log.clone()));
    for (title, log) in running.chain(stopped) {
        text.push_str(&format!("== {title} ==\n"));
        for line in log {
            text.push_str(&line);
            text.push('\n');
        }
        text.push('\n');
    }
    let id = ed.add_document(maestro_core::Document::from_text(&text));
    ed.switch_to(id);
}
