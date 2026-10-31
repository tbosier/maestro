//! An ACP agent process and the threads that talk to it.
//!
//! Five threads per agent, none of which touches editor state:
//!
//! * the dispatcher calls the caller's callback with each event, one at a
//!   time and in order, and stops after [`AcpEvent::Exited`]. Since the
//!   callback never runs on a thread that holds client state, it may call
//!   back into the client;
//! * the writer owns the agent's stdin and writes one JSON message per line
//!   from a channel. If a write fails it kills the agent, which is then
//!   reported as exited;
//! * the reader owns stdout. It routes responses to our requests, answers
//!   the agent's requests (file access, permissions, and MethodNotFound for
//!   anything else) and turns `session/update` notifications into events.
//!   When stdout closes it reports [`AcpEvent::Exited`];
//! * a watcher reports the exit when the process ends but a descendant
//!   keeps stdout open;
//! * a stderr thread forwards each line as [`AcpEvent::Stderr`].
//!
//! [`AcpClient::start`] returns as soon as the process runs; `initialize`
//! and `session/new` complete in the background and are reported as
//! [`AcpEvent::Initialized`] and [`AcpEvent::SessionStarted`]. A prompt
//! sent before that waits for the session.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, BufWriter, Read};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use agent_client_protocol_schema::ProtocolVersion;
use agent_client_protocol_schema::v1::{
    CancelNotification, ClientCapabilities, ContentBlock, FileSystemCapabilities, Implementation,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, ReadTextFileRequest, ReadTextFileResponse, RequestPermissionOutcome,
    RequestPermissionResponse, SelectedPermissionOutcome, TextContent, WriteTextFileRequest,
    WriteTextFileResponse,
};
use serde::Serialize;
use serde_json::Value;

use crate::files::{AccessError, FileAccess};
use crate::jsonrpc::{self, Line, Message};
use crate::{AcpEvent, ProviderSpec, convert, find_executable};

const STDERR_LINE_BYTES: u64 = 8 * 1024;

enum Outgoing {
    Message(Value),
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Initialize,
    NewSession,
    Prompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Turn {
    Idle,
    /// A prompt was sent (or is queued until the session starts).
    Running,
    /// `session/cancel` was sent; waiting for the prompt's answer.
    Cancelling,
}

struct State {
    /// False once the exit was reported or the client was shut down.
    alive: bool,
    pending: HashMap<i64, Pending>,
    session_id: Option<String>,
    queued_prompt: Option<String>,
    turn: Turn,
    /// Unanswered permission requests: the agent's request id and the
    /// option ids it offered.
    permissions: Vec<(Value, Vec<String>)>,
}

struct Shared {
    /// To the dispatcher thread.
    events: Sender<AcpEvent>,
    files: FileAccess,
    state: Mutex<State>,
    child: Mutex<Child>,
    out: Sender<Outgoing>,
    next_id: AtomicI64,
    exit_reported: AtomicBool,
    /// Disconnects when the stderr thread finishes.
    stderr_done: Mutex<Receiver<()>>,
    /// Disconnects when the reader thread finishes.
    reader_done: Mutex<Receiver<()>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking callback cannot leave this state half-updated in a way
    // that matters, so recover from poisoning instead of propagating it.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn to_value(v: impl Serialize) -> Value {
    serde_json::to_value(v).expect("protocol types serialize")
}

fn not_running() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "the agent is not running")
}

/// A running ACP agent with one session.
pub struct AcpClient {
    shared: Arc<Shared>,
}

impl AcpClient {
    /// Starts the agent process, runs `initialize` and `session/new` with
    /// `root` as cwd. File reads and writes the agent asks the client for
    /// are confined to `root`: paths outside it, symlinks out of it and the
    /// `.git` directory are refused.
    ///
    /// Returns once the process is running. Fails if `root` is not a
    /// directory or the program cannot be found or started. `on_event`
    /// runs on a thread of the client's, one event at a time, and should
    /// not block for long; it may call the client's methods.
    pub fn start(
        spec: &ProviderSpec,
        root: &Path,
        on_event: impl Fn(AcpEvent) + Send + Sync + 'static,
    ) -> io::Result<AcpClient> {
        let files = FileAccess::new(root)?;
        let path_var = spec
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| OsString::from(v))
            .or_else(|| std::env::var_os("PATH"));
        let program = find_executable(&spec.program, path_var.as_deref()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} was not found on PATH", spec.program),
            )
        })?;
        let mut command = Command::new(&program);
        command
            .args(&spec.args)
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .current_dir(files.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own process group, so stopping the agent also stops what it
        // started. Gemini CLI, for one, relaunches itself as a child that
        // keeps running if only the parent is killed.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (out, out_rx) = mpsc::channel();
        let (events, events_rx) = mpsc::channel();
        let (stderr_tx, stderr_rx) = mpsc::channel::<()>();
        let (reader_tx, reader_rx) = mpsc::channel::<()>();
        let shared = Arc::new(Shared {
            events,
            files,
            state: Mutex::new(State {
                alive: true,
                pending: HashMap::new(),
                session_id: None,
                queued_prompt: None,
                turn: Turn::Idle,
                permissions: Vec::new(),
            }),
            child: Mutex::new(child),
            out,
            next_id: AtomicI64::new(0),
            exit_reported: AtomicBool::new(false),
            stderr_done: Mutex::new(stderr_rx),
            reader_done: Mutex::new(reader_rx),
        });

        // Queued before any thread runs, so it is the first message.
        let init = InitializeRequest::new(ProtocolVersion::V1)
            .client_capabilities(
                ClientCapabilities::new()
                    .fs(FileSystemCapabilities::new()
                        .read_text_file(true)
                        .write_text_file(true))
                    .terminal(false),
            )
            .client_info(
                Implementation::new("maestro", env!("CARGO_PKG_VERSION")).title("Maestro"),
            );
        shared.request(
            &mut lock(&shared.state),
            Pending::Initialize,
            "initialize",
            init,
        );

        let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| {
            std::thread::Builder::new()
                .name(name.to_string())
                .spawn(f)
                .map(drop)
        };
        let started = (|| {
            spawn(
                "acp-events",
                Box::new(move || {
                    for e in events_rx {
                        let last = matches!(e, AcpEvent::Exited(_));
                        on_event(e);
                        if last {
                            // Anything sent later is dropped with the
                            // receiver.
                            break;
                        }
                    }
                }),
            )?;
            let sh = shared.clone();
            spawn(
                "acp-writer",
                Box::new(move || writer_loop(stdin, out_rx, &sh)),
            )?;
            let sh = shared.clone();
            spawn(
                "acp-stderr",
                Box::new(move || {
                    stderr_loop(stderr, &sh);
                    drop(stderr_tx);
                }),
            )?;
            let sh = shared.clone();
            spawn(
                "acp-reader",
                Box::new(move || {
                    reader_loop(stdout, &sh);
                    drop(reader_tx);
                    let code = sh.wait_or_kill(Duration::from_secs(1));
                    sh.report_exit(code);
                }),
            )?;
            let sh = shared.clone();
            spawn("acp-watcher", Box::new(move || watcher_loop(&sh)))
        })();
        let client = AcpClient { shared };
        if let Err(e) = started {
            client.shutdown();
            return Err(e);
        }
        Ok(client)
    }

    /// Sends a prompt. Fails if the agent is not running or a turn is
    /// already in progress; the turn ends with [`AcpEvent::TurnEnded`].
    pub fn prompt(&self, text: &str) -> io::Result<()> {
        let sh = &self.shared;
        let mut st = lock(&sh.state);
        if !st.alive {
            return Err(not_running());
        }
        if st.turn != Turn::Idle {
            return Err(io::Error::other("a turn is already in progress"));
        }
        st.turn = Turn::Running;
        match st.session_id.clone() {
            Some(session) => sh.send_prompt(&mut st, &session, text),
            None => st.queued_prompt = Some(text.to_string()),
        }
        Ok(())
    }

    /// Cancels the current turn: sends `session/cancel` and answers every
    /// pending permission request as cancelled. The turn still ends with
    /// [`AcpEvent::TurnEnded`] (normally `cancelled`) when the agent
    /// answers. Does nothing when no turn is running.
    pub fn cancel(&self) -> io::Result<()> {
        let sh = &self.shared;
        let mut st = lock(&sh.state);
        if !st.alive {
            return Err(not_running());
        }
        if st.turn != Turn::Running {
            return Ok(());
        }
        if st.queued_prompt.take().is_some() {
            // Never sent: nothing to tell the agent.
            st.turn = Turn::Idle;
            drop(st);
            sh.emit(AcpEvent::TurnEnded {
                stop_reason: "cancelled".to_string(),
            });
            return Ok(());
        }
        st.turn = Turn::Cancelling;
        if let Some(session) = st.session_id.clone() {
            sh.send(jsonrpc::notification(
                "session/cancel",
                to_value(CancelNotification::new(session)),
            ));
        }
        for (id, _) in std::mem::take(&mut st.permissions) {
            sh.send(permission_answer(id, None));
        }
        Ok(())
    }

    /// Answers a [`AcpEvent::PermissionRequested`] with one of its option
    /// ids, or `None` for cancelled. Fails, sending nothing, if the request
    /// is not pending (already answered, or answered by [`cancel`]) or the
    /// choice was not offered.
    ///
    /// [`cancel`]: AcpClient::cancel
    pub fn answer_permission(&self, request_id: Value, choice: Option<&str>) -> io::Result<()> {
        let sh = &self.shared;
        let mut st = lock(&sh.state);
        if !st.alive {
            return Err(not_running());
        }
        let Some(pos) = st.permissions.iter().position(|(id, _)| *id == request_id) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no pending permission request with id {request_id}"),
            ));
        };
        if let Some(c) = choice
            && !st.permissions[pos].1.iter().any(|o| o == c)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{c} is not one of the offered options"),
            ));
        }
        st.permissions.remove(pos);
        sh.send(permission_answer(request_id, choice));
        Ok(())
    }

    /// Kills the agent and, on Unix, every process in its process group
    /// (the programs it started, unless they moved to a group of their
    /// own). Its exit is still reported as [`AcpEvent::Exited`].
    pub fn shutdown(&self) {
        let sh = &self.shared;
        lock(&sh.state).alive = false;
        let _ = sh.out.send(Outgoing::Close);
        let mut child = lock(&sh.child);
        kill_tree(&mut child);
        let _ = child.wait();
    }

    /// The canonical root file access is confined to.
    pub fn root(&self) -> &Path {
        self.shared.files.root()
    }

    /// The session id once [`AcpEvent::SessionStarted`] was sent.
    pub fn session_id(&self) -> Option<String> {
        lock(&self.shared.state).session_id.clone()
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Kills the agent's process group, then the agent itself.
///
/// The group id is the agent's pid (see `start`). A group outlives its
/// leader while any member runs, so the id cannot be reused while there is
/// anything left to kill. std has no `killpg`, and this crate forbids
/// `unsafe`, so this runs kill(1).
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

fn permission_answer(id: Value, choice: Option<&str>) -> Value {
    let outcome = match choice {
        Some(c) => {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(c.to_string()))
        }
        None => RequestPermissionOutcome::Cancelled,
    };
    jsonrpc::response(id, to_value(RequestPermissionResponse::new(outcome)))
}

impl Shared {
    fn emit(&self, e: AcpEvent) {
        let _ = self.events.send(e);
    }

    fn send(&self, v: Value) {
        let _ = self.out.send(Outgoing::Message(v));
    }

    /// Sends a request of ours. Takes the state lock so the pending entry
    /// exists before the answer can arrive.
    fn request(&self, st: &mut State, kind: Pending, method: &str, params: impl Serialize) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        st.pending.insert(id, kind);
        self.send(jsonrpc::request(id, method, to_value(params)));
    }

    fn send_prompt(&self, st: &mut State, session: &str, text: &str) {
        let prompt = PromptRequest::new(
            session.to_string(),
            vec![ContentBlock::Text(TextContent::new(text))],
        );
        self.request(st, Pending::Prompt, "session/prompt", prompt);
    }

    /// The session could not be set up: report it, end a queued turn and
    /// stop the agent.
    fn fail(&self, message: String) {
        let queued = {
            let mut st = lock(&self.state);
            let queued = st.queued_prompt.take().is_some();
            if queued {
                st.turn = Turn::Idle;
            }
            queued
        };
        self.emit(AcpEvent::Error(message));
        if queued {
            self.emit(AcpEvent::TurnEnded {
                stop_reason: "error".to_string(),
            });
        }
        kill_tree(&mut lock(&self.child));
    }

    fn session_is(&self, session: &str) -> bool {
        lock(&self.state).session_id.as_deref() == Some(session)
    }

    fn handle_line(&self, bytes: &[u8]) {
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        let value: Value = match serde_json::from_slice(bytes) {
            Ok(v) => v,
            Err(e) => {
                let preview: String = String::from_utf8_lossy(bytes).chars().take(200).collect();
                self.emit(AcpEvent::Error(format!(
                    "malformed JSON from the agent ({e}): {preview}"
                )));
                return;
            }
        };
        match Message::parse(value) {
            Some(Message::Response { id, result }) => self.on_response(id, result),
            Some(Message::Request { id, method, params }) => self.on_request(id, &method, params),
            Some(Message::Notification { method, params }) => self.on_notification(&method, params),
            None => self.emit(AcpEvent::Error(
                "the agent sent JSON that is not a JSON-RPC message".to_string(),
            )),
        }
    }

    fn on_response(&self, id: Value, result: Result<Value, jsonrpc::RpcError>) {
        let Some(id) = id.as_i64() else {
            return;
        };
        let Some(kind) = lock(&self.state).pending.remove(&id) else {
            return;
        };
        match kind {
            Pending::Initialize => {
                let response = match result {
                    Ok(v) => serde_json::from_value::<InitializeResponse>(v)
                        .map_err(|e| format!("malformed initialize response: {e}")),
                    Err(e) => Err(format!("initialize failed: {e}")),
                };
                let r = match response {
                    Ok(r) => r,
                    Err(message) => return self.fail(message),
                };
                if r.protocol_version != ProtocolVersion::V1 {
                    return self.fail(format!(
                        "the agent speaks ACP version {}; maestro speaks version 1",
                        r.protocol_version
                    ));
                }
                let agent_name = r.agent_info.map(|i| i.title.unwrap_or(i.name));
                let session = NewSessionRequest::new(self.files.root());
                self.request(
                    &mut lock(&self.state),
                    Pending::NewSession,
                    "session/new",
                    session,
                );
                self.emit(AcpEvent::Initialized {
                    agent_name,
                    load_session: r.agent_capabilities.load_session,
                });
            }
            Pending::NewSession => {
                let response = match result {
                    Ok(v) => serde_json::from_value::<NewSessionResponse>(v)
                        .map_err(|e| format!("malformed session/new response: {e}")),
                    Err(e) => Err(format!("session/new failed: {e}")),
                };
                let session_id = match response {
                    Ok(r) => r.session_id.0.to_string(),
                    Err(message) => return self.fail(message),
                };
                {
                    let mut st = lock(&self.state);
                    st.session_id = Some(session_id.clone());
                    if let Some(text) = st.queued_prompt.take() {
                        self.send_prompt(&mut st, &session_id, &text);
                    }
                }
                self.emit(AcpEvent::SessionStarted { session_id });
            }
            Pending::Prompt => {
                let stop_reason = match result {
                    Ok(v) => match serde_json::from_value::<PromptResponse>(v) {
                        Ok(r) => match serde_json::to_value(r.stop_reason) {
                            Ok(Value::String(s)) => s,
                            _ => "unknown".to_string(),
                        },
                        Err(e) => {
                            self.emit(AcpEvent::Error(format!("malformed prompt response: {e}")));
                            "error".to_string()
                        }
                    },
                    Err(e) => {
                        self.emit(AcpEvent::Error(format!("the prompt failed: {e}")));
                        "error".to_string()
                    }
                };
                let stale = {
                    let mut st = lock(&self.state);
                    st.turn = Turn::Idle;
                    std::mem::take(&mut st.permissions)
                };
                // The turn is over; nothing can still be waiting on these.
                for (id, _) in stale {
                    self.send(permission_answer(id, None));
                }
                self.emit(AcpEvent::TurnEnded { stop_reason });
            }
        }
    }

    fn on_request(&self, id: Value, method: &str, params: Value) {
        let reply = match method {
            "session/request_permission" => {
                match convert::permission_request(id.clone(), params) {
                    Err(e) => jsonrpc::error_response(id, jsonrpc::INVALID_PARAMS, &e),
                    Ok((session, request)) => {
                        let mut st = lock(&self.state);
                        if st.session_id.as_deref() != Some(session.as_str()) {
                            jsonrpc::error_response(id, jsonrpc::INVALID_PARAMS, "unknown session")
                        } else if st.turn == Turn::Cancelling {
                            permission_answer(id, None)
                        } else {
                            let options = request.options.iter().map(|o| o.id.clone()).collect();
                            // A repeated id replaces the earlier request.
                            st.permissions.retain(|(pending, _)| *pending != id);
                            st.permissions.push((id, options));
                            drop(st);
                            self.emit(AcpEvent::PermissionRequested(request));
                            return;
                        }
                    }
                }
            }
            "fs/read_text_file" => match serde_json::from_value::<ReadTextFileRequest>(params) {
                Err(e) => jsonrpc::error_response(id, jsonrpc::INVALID_PARAMS, &e.to_string()),
                Ok(r) if !self.session_is(&r.session_id.0) => {
                    jsonrpc::error_response(id, jsonrpc::INVALID_PARAMS, "unknown session")
                }
                Ok(r) => match self.files.read(&r.path, r.line, r.limit) {
                    Ok(content) => {
                        jsonrpc::response(id, to_value(ReadTextFileResponse::new(content)))
                    }
                    Err(e) => self.refuse(id, "read", &e),
                },
            },
            "fs/write_text_file" => match serde_json::from_value::<WriteTextFileRequest>(params) {
                Err(e) => jsonrpc::error_response(id, jsonrpc::INVALID_PARAMS, &e.to_string()),
                Ok(r) if !self.session_is(&r.session_id.0) => {
                    jsonrpc::error_response(id, jsonrpc::INVALID_PARAMS, "unknown session")
                }
                Ok(r) => match self.files.write(&r.path, &r.content) {
                    Ok(path) => {
                        self.send(jsonrpc::response(
                            id,
                            to_value(WriteTextFileResponse::new()),
                        ));
                        self.emit(AcpEvent::FileWritten(path));
                        return;
                    }
                    Err(e) => self.refuse(id, "write", &e),
                },
            },
            _ => jsonrpc::error_response(
                id,
                jsonrpc::METHOD_NOT_FOUND,
                &format!("maestro does not handle {method}"),
            ),
        };
        self.send(reply);
    }

    /// The error reply for a failed file request. Refusals for confinement
    /// reasons, and every failed write, are also reported as events.
    fn refuse(&self, id: Value, what: &str, e: &AccessError) -> Value {
        let code = match e {
            AccessError::NotFound(_) => jsonrpc::RESOURCE_NOT_FOUND,
            AccessError::Io(_) => jsonrpc::INTERNAL_ERROR,
            _ => jsonrpc::INVALID_PARAMS,
        };
        let notable = matches!(e, AccessError::OutsideRoot(_) | AccessError::GitDir(_));
        if notable || what == "write" {
            self.emit(AcpEvent::Error(format!("refused the agent's {what}: {e}")));
        }
        jsonrpc::error_response(id, code, &e.to_string())
    }

    fn on_notification(&self, method: &str, params: Value) {
        if method != "session/update" {
            return;
        }
        match convert::session_update(params) {
            Ok(Some((session, event))) => {
                if self.session_is(&session) {
                    self.emit(event);
                } else {
                    self.emit(AcpEvent::Error(format!(
                        "ignored an update for unknown session {session}"
                    )));
                }
            }
            Ok(None) => {}
            Err(e) => self.emit(AcpEvent::Error(e)),
        }
    }

    /// Waits up to `timeout` for the process to exit, then kills it.
    /// Returns the exit code, `None` for a signal or an unknown status.
    fn wait_or_kill(&self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            match lock(&self.child).try_wait() {
                Ok(Some(status)) => return status.code(),
                Ok(None) => {}
                Err(_) => return None,
            }
            if Instant::now() >= deadline {
                let mut child = lock(&self.child);
                kill_tree(&mut child);
                return child.wait().ok().and_then(|s| s.code());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Marks the agent dead and sends [`AcpEvent::Exited`] once, from
    /// whichever thread noticed first.
    fn report_exit(&self, code: Option<i32>) {
        if self.exit_reported.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut st = lock(&self.state);
            st.alive = false;
            st.pending.clear();
            st.queued_prompt = None;
            st.permissions.clear();
            st.turn = Turn::Idle;
        }
        let _ = self.out.send(Outgoing::Close);
        // The agent is gone; so are the programs it left running in its
        // process group (which may be what still holds stdout open).
        kill_tree(&mut lock(&self.child));
        // Let the last stderr lines, often the reason for the exit, arrive
        // first. A descendant holding stderr open must not delay this long.
        let _ = lock(&self.stderr_done).recv_timeout(Duration::from_millis(200));
        self.emit(AcpEvent::Exited(code));
    }
}

fn writer_loop(stdin: ChildStdin, rx: Receiver<Outgoing>, sh: &Shared) {
    let mut w = BufWriter::new(stdin);
    for msg in rx {
        let Outgoing::Message(v) = msg else {
            break;
        };
        if let Err(e) = jsonrpc::write_line(&mut w, &v) {
            // An agent that stopped reading cannot be kept in sync. Kill it
            // so the exit is reported like any other.
            if !sh.exit_reported.load(Ordering::SeqCst) {
                sh.emit(AcpEvent::Error(format!("writing to the agent failed: {e}")));
                kill_tree(&mut lock(&sh.child));
            }
            break;
        }
    }
}

fn reader_loop(stdout: impl Read, sh: &Shared) {
    let mut r = BufReader::new(stdout);
    loop {
        match jsonrpc::read_line(&mut r) {
            Ok(Line::Text(bytes)) => sh.handle_line(&bytes),
            Ok(Line::Eof) => break,
            Ok(Line::TooLong) => {
                sh.emit(AcpEvent::Error(format!(
                    "the agent sent a message longer than {} bytes; stopping it",
                    jsonrpc::MAX_LINE
                )));
                kill_tree(&mut lock(&sh.child));
                break;
            }
            Err(e) => {
                sh.emit(AcpEvent::Error(format!(
                    "reading from the agent failed: {e}"
                )));
                break;
            }
        }
    }
}

fn stderr_loop(stderr: impl Read, sh: &Shared) {
    let mut r = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Bounded: a longer line is split.
        match (&mut r).take(STDERR_LINE_BYTES).read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                sh.emit(AcpEvent::Stderr(
                    line.trim_end_matches(['\r', '\n']).to_string(),
                ));
            }
        }
    }
}

/// The reader only notices an exit when stdout closes, which a descendant
/// that inherited stdout can delay forever. This notices the process
/// itself exiting.
fn watcher_loop(sh: &Shared) {
    loop {
        if sh.exit_reported.load(Ordering::SeqCst) {
            return;
        }
        let status = lock(&sh.child).try_wait();
        match status {
            Ok(Some(status)) => {
                // Give the reader time to deliver what the agent wrote
                // before exiting; it reports the exit itself if stdout
                // closes in that time.
                let _ = lock(&sh.reader_done).recv_timeout(Duration::from_millis(500));
                sh.report_exit(status.code());
                return;
            }
            Ok(None) => {}
            Err(_) => {
                sh.report_exit(None);
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
