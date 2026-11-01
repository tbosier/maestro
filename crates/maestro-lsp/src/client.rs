//! A language server process and the threads that talk to it.
//!
//! Four threads per server, none of which ever touches editor state:
//!
//! * the writer owns the server's stdin and writes framed messages from a
//!   channel. Until the `initialize` response arrives it holds ordinary
//!   messages back, then sends `initialized` followed by the backlog. If a
//!   write fails it kills the server, which is then reported as exited;
//! * the reader owns stdout, routes responses to the channel of the request
//!   that asked, answers most server-to-client requests itself, and hands
//!   notifications and the requests in [`FORWARDED_REQUESTS`] to the
//!   caller's sink. The caller answers those with [`Client::respond`]. When
//!   stdout closes it reports [`Event::Exited`];
//! * a watcher reports the exit when the process ends but a descendant
//!   keeps stdout open;
//! * a stderr thread keeps the last [`LOG_LINES`] lines in memory, so server
//!   noise never reaches the terminal.
//!
//! The caller never waits for the server: requests return a channel.
//! [`Client::start`] does spawn the process on the caller's thread, and
//! dropping a client waits up to half a second per shutdown step.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, BufWriter, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::convert::path_to_uri;
use crate::framing::{read_message, write_message};
use crate::jsonrpc::{self, Message, RequestId, RpcError};

/// Lines of stderr and `window/logMessage` kept per server.
pub const LOG_LINES: usize = 2000;
const LOG_LINE_CHARS: usize = 2000;

/// The answer to a request.
pub type Reply = Result<Value, RpcError>;

/// Server-to-client requests the reader hands to the caller instead of
/// answering itself. The caller must answer each with [`Client::respond`].
pub const FORWARDED_REQUESTS: &[&str] = &["workspace/applyEdit"];

/// Something the server did that the editor should hear about.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A notification other than logging and progress.
    Notification { method: String, params: Value },
    /// A request in [`FORWARDED_REQUESTS`]. `id` is the server's, echoed
    /// back verbatim by [`Client::respond`].
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// The server's stdout closed. `status` describes the exit.
    Exited { status: String },
}

/// Receives [`Event`]s on the reader thread. It must not block for long.
pub type EventSink = Box<dyn FnMut(Event) + Send>;

pub struct StartOptions {
    /// The executable, already resolved (see [`find_executable`]).
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Workspace root: the server's working directory and `rootUri`.
    pub root: PathBuf,
}

enum Outgoing {
    /// Held back until the server is initialized.
    Queued(Value),
    /// Written immediately (initialize, replies to server requests).
    Now(Value),
    /// The initialize response arrived: send `initialized` and the backlog.
    Ready,
    Close,
}

struct Shared {
    next_id: AtomicI64,
    pending: Mutex<HashMap<RequestId, Sender<Reply>>>,
    alive: AtomicBool,
    /// Set once [`Event::Exited`] has been sent, or on an intended shutdown.
    exit_reported: AtomicBool,
    stderr_done: AtomicBool,
    log: Mutex<VecDeque<String>>,
    capabilities: Mutex<Option<Value>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking thread cannot leave these maps half-updated in a way that
    // matters, so recover from poisoning instead of propagating it.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn push_log(&self, line: &str) {
        let mut log = lock(&self.log);
        if log.len() >= LOG_LINES {
            log.pop_front();
        }
        log.push_back(line.chars().take(LOG_LINE_CHARS).collect());
    }
}

pub struct Client {
    out: Sender<Outgoing>,
    shared: Arc<Shared>,
    child: Arc<Mutex<Child>>,
    shut_down: bool,
}

const INITIALIZE_ID: RequestId = 0;

impl Client {
    /// Spawns the server and sends `initialize`. Returns at once with the
    /// client and a channel that yields the `initialize` result.
    pub fn start(
        opts: StartOptions,
        sink: EventSink,
    ) -> std::io::Result<(Client, Receiver<Reply>)> {
        let mut child = Command::new(&opts.program)
            .args(&opts.args)
            .current_dir(&opts.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let shared = Arc::new(Shared {
            next_id: AtomicI64::new(INITIALIZE_ID + 1),
            pending: Mutex::new(HashMap::new()),
            alive: AtomicBool::new(true),
            exit_reported: AtomicBool::new(false),
            stderr_done: AtomicBool::new(false),
            log: Mutex::new(VecDeque::new()),
            capabilities: Mutex::new(None),
        });
        let child = Arc::new(Mutex::new(child));
        let sink = Arc::new(Mutex::new(sink));
        let (out, out_rx) = mpsc::channel::<Outgoing>();
        let (init_tx, init_rx) = mpsc::channel();
        lock(&shared.pending).insert(INITIALIZE_ID, init_tx);

        {
            let shared = shared.clone();
            let child = child.clone();
            std::thread::Builder::new()
                .name("lsp-writer".into())
                .spawn(move || writer_loop(stdin, out_rx, &shared, &child))?;
        }

        {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("lsp-stderr".into())
                .spawn(move || {
                    let mut r = BufReader::new(stderr);
                    let mut buf = Vec::new();
                    loop {
                        buf.clear();
                        // Bounded: a line without a newline is split.
                        let limit = (LOG_LINE_CHARS * 4) as u64;
                        match (&mut r).take(limit).read_until(b'\n', &mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let line = String::from_utf8_lossy(&buf);
                                shared.push_log(line.trim_end_matches(['\r', '\n']));
                            }
                        }
                    }
                    shared.stderr_done.store(true, Ordering::SeqCst);
                })?;
        }

        {
            let shared = shared.clone();
            let out = out.clone();
            let child = child.clone();
            let sink = sink.clone();
            std::thread::Builder::new()
                .name("lsp-reader".into())
                .spawn(move || {
                    let mut r = BufReader::new(stdout);
                    loop {
                        match read_message(&mut r) {
                            Ok(Some(v)) => {
                                if let Some(msg) = Message::parse(v) {
                                    route(msg, &shared, &out, &sink);
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                shared.push_log(&format!("[maestro] read error: {e}"));
                                break;
                            }
                        }
                    }
                    let status = wait_for_exit(&child, Duration::from_secs(1))
                        .unwrap_or_else(|| "output closed".to_string());
                    report_exit(&shared, &out, &sink, status);
                })?;
        }

        // The reader only notices an exit when stdout closes, which a
        // descendant that inherited stdout can delay forever. This thread
        // notices the process itself exiting.
        {
            let shared = shared.clone();
            let out = out.clone();
            let child = child.clone();
            std::thread::Builder::new()
                .name("lsp-watcher".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(Duration::from_millis(250));
                        if shared.exit_reported.load(Ordering::SeqCst) {
                            return;
                        }
                        if let Ok(Some(status)) = lock(&child).try_wait() {
                            // Give the reader a moment to report it with
                            // the output that preceded the exit.
                            std::thread::sleep(Duration::from_millis(300));
                            report_exit(&shared, &out, &sink, describe(status));
                            return;
                        }
                    }
                })?;
        }

        let root_uri = path_to_uri(&opts.root);
        let folder_name = opts
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root_uri.clone());
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "maestro", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": root_uri,
            "rootPath": opts.root.to_string_lossy(),
            "workspaceFolders": [{"uri": root_uri, "name": folder_name}],
            "capabilities": {
                "general": {"positionEncodings": ["utf-16"]},
                "textDocument": {
                    "synchronization": {"didSave": true, "dynamicRegistration": false},
                    "publishDiagnostics": {"versionSupport": true},
                    "hover": {"contentFormat": ["plaintext", "markdown"]},
                    "definition": {"linkSupport": true},
                    "references": {},
                    "rename": {"prepareSupport": true},
                    "formatting": {},
                    "rangeFormatting": {},
                    // Actions come as literals; an action without an edit
                    // is resolved with `codeAction/resolve`.
                    "codeAction": {
                        "codeActionLiteralSupport": {
                            "codeActionKind": {"valueSet": [
                                "", "quickfix", "refactor", "refactor.extract",
                                "refactor.inline", "refactor.rewrite", "source",
                                "source.organizeImports",
                            ]},
                        },
                        "isPreferredSupport": true,
                        "disabledSupport": true,
                        "dataSupport": true,
                        "resolveSupport": {"properties": ["edit"]},
                    },
                    "signatureHelp": {
                        "signatureInformation": {
                            "parameterInformation": {"labelOffsetSupport": true},
                            "activeParameterSupport": true,
                        },
                        "contextSupport": true,
                    },
                    // Snippets are accepted as plain text (placeholders
                    // stripped); no property is resolved lazily, so
                    // servers send `additionalTextEdits` up front.
                    "completion": {
                        "completionItem": {
                            "snippetSupport": true,
                            "insertReplaceSupport": true,
                        },
                        "contextSupport": true,
                    },
                },
                // Edits are all or nothing: any problem refuses the whole
                // edit. Creating, renaming and deleting files is not
                // supported, so no resource operations are listed.
                "workspace": {
                    "workspaceFolders": true,
                    "configuration": true,
                    "applyEdit": true,
                    "workspaceEdit": {
                        "documentChanges": true,
                        "failureHandling": "transactional",
                    },
                    "executeCommand": {},
                },
                "window": {"workDoneProgress": false},
            },
        });
        let _ = out.send(Outgoing::Now(jsonrpc::request(
            INITIALIZE_ID,
            "initialize",
            params,
        )));
        Ok((
            Client {
                out,
                shared,
                child,
                shut_down: false,
            },
            init_rx,
        ))
    }

    /// Sends a request. The receiver yields the reply, or disconnects if the
    /// request is cancelled or the server dies.
    pub fn request(&self, method: &str, params: Value) -> (RequestId, Receiver<Reply>) {
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        if self.is_alive() {
            lock(&self.shared.pending).insert(id, tx);
            let _ = self
                .out
                .send(Outgoing::Queued(jsonrpc::request(id, method, params)));
        }
        (id, rx)
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self
            .out
            .send(Outgoing::Queued(jsonrpc::notification(method, params)));
    }

    /// Answers a request the server sent ([`Event::Request`]).
    pub fn respond(&self, id: Value, result: Result<Value, RpcError>) {
        let msg = match result {
            Ok(v) => jsonrpc::response(id, v),
            Err(e) => jsonrpc::error_response(id, e.code, &e.message),
        };
        let _ = self.out.send(Outgoing::Now(msg));
    }

    /// Abandons a request: its receiver disconnects and the server is sent
    /// `$/cancelRequest`. A reply that still arrives is discarded.
    pub fn cancel(&self, id: RequestId) {
        if lock(&self.shared.pending).remove(&id).is_some() {
            self.notify("$/cancelRequest", json!({"id": id}));
        }
    }

    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }

    /// The server's capabilities once `initialize` has been answered.
    pub fn capabilities(&self) -> Option<Value> {
        lock(&self.shared.capabilities).clone()
    }

    /// The captured stderr and log messages, oldest first.
    pub fn log(&self) -> Vec<String> {
        lock(&self.shared.log).iter().cloned().collect()
    }

    /// Asks the server to shut down and exit, waiting at most `timeout` for
    /// each step before killing the process.
    pub fn shutdown(&mut self, timeout: Duration) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;
        // An exit we asked for is not news.
        self.shared.exit_reported.store(true, Ordering::SeqCst);
        if self.is_alive() && self.capabilities().is_some() {
            let (_, rx) = self.request("shutdown", Value::Null);
            let _ = rx.recv_timeout(timeout);
            self.notify("exit", Value::Null);
        }
        let _ = self.out.send(Outgoing::Close);
        if wait_for_exit(&self.child, timeout).is_none() {
            let _ = lock(&self.child).kill();
            let _ = lock(&self.child).wait();
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown(Duration::from_millis(500));
    }
}

/// Marks the server dead and sends [`Event::Exited`] once, from whichever
/// thread noticed first.
fn report_exit(shared: &Shared, out: &Sender<Outgoing>, sink: &Mutex<EventSink>, status: String) {
    shared.alive.store(false, Ordering::SeqCst);
    // Dropping the senders wakes every waiter with a disconnect.
    lock(&shared.pending).clear();
    let _ = out.send(Outgoing::Close);
    if shared.exit_reported.swap(true, Ordering::SeqCst) {
        return;
    }
    // Let the last stderr lines (often the reason for the exit) reach the
    // log first. A grandchild holding stderr open must not delay the report
    // for long.
    let deadline = Instant::now() + Duration::from_millis(200);
    while !shared.stderr_done.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    (lock(sink))(Event::Exited { status });
}

fn writer_loop(stdin: ChildStdin, rx: Receiver<Outgoing>, shared: &Shared, child: &Mutex<Child>) {
    let mut w = BufWriter::new(stdin);
    let mut ready = false;
    let mut backlog = Vec::new();
    for msg in rx {
        let result = match msg {
            Outgoing::Now(v) => write_message(&mut w, &v),
            Outgoing::Queued(v) if ready => write_message(&mut w, &v),
            Outgoing::Queued(v) => {
                backlog.push(v);
                Ok(())
            }
            Outgoing::Ready => {
                ready = true;
                std::iter::once(jsonrpc::notification("initialized", json!({})))
                    .chain(backlog.drain(..))
                    .try_for_each(|v| write_message(&mut w, &v))
            }
            Outgoing::Close => break,
        };
        if let Err(e) = result {
            // A server that stopped reading cannot be kept in sync. Kill it
            // so the exit is reported like any other crash.
            if !shared.exit_reported.load(Ordering::SeqCst) {
                shared.push_log(&format!("[maestro] write to server failed: {e}"));
                let _ = lock(child).kill();
            }
            break;
        }
    }
}

fn route(msg: Message, shared: &Shared, out: &Sender<Outgoing>, sink: &Mutex<EventSink>) {
    match msg {
        Message::Response { id, result } => {
            let Some(id) = id.as_i64() else {
                return;
            };
            if id == INITIALIZE_ID
                && let Ok(v) = &result
            {
                let caps = v.get("capabilities").cloned().unwrap_or(json!({}));
                *lock(&shared.capabilities) = Some(caps);
                let _ = out.send(Outgoing::Ready);
            }
            let tx = lock(&shared.pending).remove(&id);
            if let Some(tx) = tx {
                let _ = tx.send(result);
            }
        }
        Message::Request { id, method, params } => {
            if FORWARDED_REQUESTS.contains(&method.as_str()) {
                (lock(sink))(Event::Request { id, method, params });
                return;
            }
            let reply = match method.as_str() {
                "workspace/configuration" => {
                    let n = params
                        .get("items")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    jsonrpc::response(id, Value::Array(vec![Value::Null; n]))
                }
                "client/registerCapability"
                | "client/unregisterCapability"
                | "window/workDoneProgress/create"
                | "window/showMessageRequest"
                | "workspace/semanticTokens/refresh"
                | "workspace/inlayHint/refresh"
                | "workspace/codeLens/refresh"
                | "workspace/diagnostic/refresh" => jsonrpc::response(id, Value::Null),
                _ => jsonrpc::error_response(
                    id,
                    RpcError::METHOD_NOT_FOUND,
                    &format!("maestro does not handle {method}"),
                ),
            };
            let _ = out.send(Outgoing::Now(reply));
        }
        Message::Notification { method, params } => match method.as_str() {
            "window/logMessage" => {
                let text = params.get("message").and_then(Value::as_str).unwrap_or("");
                for line in text.lines() {
                    shared.push_log(line);
                }
            }
            "$/progress" | "$/logTrace" | "telemetry/event" => {}
            _ => (lock(sink))(Event::Notification { method, params }),
        },
    }
}

fn describe(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(c) => format!("exit code {c}"),
        None => "killed by a signal".to_string(),
    }
}

/// Waits up to `timeout` for the process to exit and describes the result.
/// Returns `None` if it is still running.
fn wait_for_exit(child: &Mutex<Child>, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        match lock(child).try_wait() {
            Ok(Some(status)) => return Some(describe(status)),
            Ok(None) => {}
            Err(e) => return Some(format!("unknown status ({e})")),
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits for a reply, treating a timeout or a dropped request alike.
pub fn recv_reply(rx: &Receiver<Reply>, timeout: Duration) -> Option<Reply> {
    rx.recv_timeout(timeout).ok()
}

/// Finds `name` the way a shell would: a name containing a path separator
/// is used as given; otherwise each `PATH` entry is tried. Only regular,
/// executable files count.
pub fn find_executable(name: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        // The server runs with the workspace root as its working directory,
        // so a relative path must be resolved here, against the directory
        // where it was checked, or a file in the workspace could run
        // instead.
        let absolute = std::path::absolute(candidate).ok()?;
        return is_executable(&absolute).then_some(absolute);
    }
    let path_var = path_var?;
    // Relative PATH entries are skipped for the same reason.
    std::env::split_paths(path_var)
        .filter(|dir| dir.is_absolute())
        .flat_map(|dir| {
            let plain = dir.join(name);
            let exe = cfg!(windows).then(|| dir.join(format!("{name}.exe")));
            std::iter::once(plain).chain(exe)
        })
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_executable_searches_path_and_skips_missing() {
        let sh = find_executable("sh", std::env::var_os("PATH").as_deref());
        if cfg!(unix) {
            assert!(sh.is_some());
        }
        assert!(
            find_executable(
                "maestro-definitely-not-installed",
                std::env::var_os("PATH").as_deref()
            )
            .is_none()
        );
        assert!(find_executable("", None).is_none());
        // Relative names come back absolute; relative PATH entries are ignored.
        if cfg!(unix) {
            let sh = sh.unwrap();
            let dir = sh.parent().unwrap();
            // A relative spelling of `dir`: up to `/`, then down again.
            let depth = std::env::current_dir().unwrap().components().count() - 1;
            let up = "../".repeat(depth);
            let rel = format!(
                "{up}{}/sh",
                dir.display().to_string().trim_start_matches('/')
            );
            let found = find_executable(&rel, None).unwrap();
            assert!(found.is_absolute(), "{found:?}");
            assert!(find_executable("sh", Some(OsStr::new("relative/bin"))).is_none());
        }
        assert!(find_executable("/nonexistent/dir/tool", None).is_none());
    }
}
