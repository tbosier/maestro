//! A scriptable fake ACP agent for tests. Not used by the editor.
//!
//! Run it as the `fake-acp-agent` binary (built with the `fake-agent`
//! feature), or from any test binary that calls [`run_if_requested`] first
//! in `main` and starts itself with [`FAKE_ARG`]. It speaks real
//! newline-delimited JSON-RPC on stdio and checks the client's messages as
//! raw JSON, independently of the schema types the client uses.
//!
//! `initialize` is answered with protocol version 1 and agent name
//! `fake-acp-agent`; `session/new` with session id `fake-session`.
//!
//! # Command language
//!
//! The text of each `session/prompt` is a script: one command per line,
//! run in order. Blank lines are skipped. In `<text>` arguments, `\n`
//! stands for a newline. Relative paths are taken from the session's cwd.
//!
//! | Command | What the agent does |
//! |---|---|
//! | `say <text>` | sends an `agent_message_chunk` |
//! | `think <text>` | sends an `agent_thought_chunk` |
//! | `user <text>` | sends a `user_message_chunk` |
//! | `plan <a> \| <b> ...` | sends a plan; an entry starting with `*` is `completed`, others `pending` |
//! | `tool <id> <kind> <path> <title>` | sends a `tool_call` (status `pending`) with one location |
//! | `tool-update <id> <status>` | sends a `tool_call_update` with a new status |
//! | `write <path> <text>` | calls `fs/write_text_file`, then says `wrote <path>` or `write failed: <code> <message>` |
//! | `read <path> [line] [limit]` | calls `fs/read_text_file`, then says `read: <content>` or `read failed: <code> <message>` |
//! | `write-raw <path> <text>` | like `write`, with the path sent exactly as given (relative paths stay relative) |
//! | `ask <title>` | calls `session/request_permission` (options `allow`: allow_once, `reject`: reject_once), then says `permission: <option>`; a cancelled answer says `permission: cancelled` (or `permission: cancelled after session/cancel` when that notification came first) and ends the turn as `cancelled` |
//! | `ask-dup <title>` | sends two permission requests with the same id before either is answered; says `duplicate: <error code>` and `permission: <option>` as the answers arrive |
//! | `fake-response` | writes `{"id": <id of the current prompt>}`, which is not a valid response |
//! | `unversioned-write <path>` | writes an `fs/write_text_file` request without `"jsonrpc": "2.0"` and does not wait for an answer |
//! | `wait-cancel` | waits for `session/cancel`, then ends the turn as `cancelled` |
//! | `unknown-request` | calls `fake/unknown`, then says `unknown: <error code>` |
//! | `stderr <text>` | writes a line to stderr |
//! | `garbage` | writes a line that is not JSON |
//! | `not-rpc` | writes a JSON line that is not a JSON-RPC message |
//! | `update-unknown` | sends updates of kinds the client ignores (`available_commands_update`, `current_mode_update`, a made-up kind) |
//! | `bad-update` | sends a `tool_call` update without its required fields |
//! | `wrong-session` | sends an `agent_message_chunk` for another session |
//! | `caps` | says the `clientCapabilities` from `initialize`, as JSON |
//! | `cwd` | says the cwd from `session/new` |
//! | `spawn-child` | starts `sleep 30` sharing the agent's stdout and stderr, then says `child <pid>` |
//! | `orphan` | like `spawn-child`, then exits with code 4 while the child keeps stdout open |
//! | `crash` | exits with code 3 at once |
//! | `fail` | answers the prompt with a JSON-RPC error |
//! | `stop <reason>` | ends the turn with this stop reason |
//!
//! A turn ends with `end_turn` after the last command, or `cancelled` as
//! soon as a `session/cancel` has arrived.
//!
//! Extra arguments change the handshake: [`FAIL_SESSION_ARG`] answers
//! `session/new` with an authentication error, [`SLOW_SESSION_ARG`]
//! answers it after 300 ms, and [`PROTOCOL_ARG`] followed by a number
//! answers `initialize` with that protocol version.
//!
//! With [`LOAD_SESSION_ARG`] the agent advertises `loadSession` and answers
//! `session/load`. Only [`SESSION_ID`] can be loaded: the agent first
//! replays a `user_message_chunk` saying `earlier prompt` and an
//! `agent_message_chunk` saying `replayed <session id>`, then answers with
//! an empty result. Any other session id gets a `-32002` "session not
//! found" error. Without the argument, `session/load` is method not found.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

pub const FAKE_ARG: &str = "--maestro-fake-acp";
pub const FAIL_SESSION_ARG: &str = "--fake-fail-session";
pub const SLOW_SESSION_ARG: &str = "--fake-slow-session";
pub const PROTOCOL_ARG: &str = "--fake-protocol";
pub const LOAD_SESSION_ARG: &str = "--fake-load-session";
pub const SESSION_ID: &str = "fake-session";

/// If this process was started with [`FAKE_ARG`], serves stdio and exits.
pub fn run_if_requested() {
    if std::env::args().any(|a| a == FAKE_ARG) {
        std::process::exit(serve());
    }
}

/// Serves stdio until stdin closes. Returns the exit code.
pub fn serve() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let protocol = args
        .iter()
        .position(|a| a == PROTOCOL_ARG)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1);
    let mut agent = Agent {
        input: BufReader::new(std::io::stdin().lock()),
        next_id: 0,
        prompt_id: Value::Null,
        cwd: PathBuf::new(),
        session: None,
        client_caps: Value::Null,
        cancelled: false,
        protocol,
        fail_session: args.iter().any(|a| a == FAIL_SESSION_ARG),
        slow_session: args.iter().any(|a| a == SLOW_SESSION_ARG),
        load_session: args.iter().any(|a| a == LOAD_SESSION_ARG),
    };
    eprintln!("fake-acp-agent: started");
    agent.run()
}

enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        result: Result<Value, Value>,
    },
}

struct Agent {
    /// The id of the `session/prompt` being served.
    prompt_id: Value,
    input: BufReader<std::io::StdinLock<'static>>,
    next_id: i64,
    cwd: PathBuf,
    session: Option<String>,
    client_caps: Value,
    cancelled: bool,
    protocol: u64,
    fail_session: bool,
    slow_session: bool,
    load_session: bool,
}

/// How a script ended.
enum End {
    Stop(String),
    Error(String),
}

fn send(v: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn send_raw(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn reply(id: Value, result: Value) {
    send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn reply_error(id: Value, code: i64, message: &str) {
    send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
}

fn unescape(s: &str) -> String {
    s.replace("\\n", "\n")
}

impl Agent {
    fn run(&mut self) -> i32 {
        while let Some(msg) = self.next() {
            match msg {
                Incoming::Request { id, method, params } => self.on_request(id, &method, params),
                Incoming::Notification { .. } | Incoming::Response { .. } => {}
            }
        }
        0
    }

    /// The next message from the client, or `None` when stdin closes.
    /// Lines that are not JSON-RPC are logged and skipped.
    fn next(&mut self) -> Option<Incoming> {
        loop {
            let mut line = String::new();
            match self.input.read_line(&mut line) {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("fake-acp-agent: bad input: {e}");
                    continue;
                }
            };
            if v["jsonrpc"] != "2.0" {
                eprintln!("fake-acp-agent: missing jsonrpc 2.0: {v}");
            }
            let method = v["method"].as_str().map(str::to_string);
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            return Some(match (method, v.get("id").cloned()) {
                (Some(method), Some(id)) => Incoming::Request { id, method, params },
                (Some(method), None) => Incoming::Notification { method, params },
                (None, Some(id)) => Incoming::Response {
                    id,
                    result: match v.get("error") {
                        Some(e) if !e.is_null() => Err(e.clone()),
                        _ => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                    },
                },
                (None, None) => {
                    eprintln!("fake-acp-agent: not a JSON-RPC message: {v}");
                    continue;
                }
            });
        }
    }

    fn on_request(&mut self, id: Value, method: &str, params: Value) {
        match method {
            "initialize" => {
                if params["protocolVersion"] != 1 {
                    return reply_error(id, -32602, "expected protocolVersion 1");
                }
                self.client_caps = params["clientCapabilities"].clone();
                reply(
                    id,
                    json!({
                        "protocolVersion": self.protocol,
                        "agentCapabilities": {"loadSession": self.load_session},
                        "agentInfo": {"name": "fake-acp-agent", "version": "0.0.0"},
                        "authMethods": [],
                    }),
                );
            }
            "session/new" => {
                if self.slow_session {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
                if self.fail_session {
                    return reply_error(id, -32000, "Authentication required");
                }
                let Some(cwd) = params["cwd"].as_str() else {
                    return reply_error(id, -32602, "session/new without cwd");
                };
                if !params["mcpServers"].is_array() {
                    return reply_error(id, -32602, "session/new without mcpServers");
                }
                self.cwd = PathBuf::from(cwd);
                if !self.cwd.is_absolute() {
                    return reply_error(id, -32602, "cwd must be absolute");
                }
                self.session = Some(SESSION_ID.to_string());
                reply(id, json!({"sessionId": SESSION_ID}));
            }
            "session/load" if self.load_session => {
                let Some(cwd) = params["cwd"].as_str() else {
                    return reply_error(id, -32602, "session/load without cwd");
                };
                if !params["mcpServers"].is_array() {
                    return reply_error(id, -32602, "session/load without mcpServers");
                }
                if params["sessionId"].as_str() != Some(SESSION_ID) {
                    return reply_error(id, -32002, "session not found");
                }
                self.cwd = PathBuf::from(cwd);
                if !self.cwd.is_absolute() {
                    return reply_error(id, -32602, "cwd must be absolute");
                }
                self.session = Some(SESSION_ID.to_string());
                // The conversation so far comes back as updates before the
                // answer.
                self.update(json!({
                    "sessionUpdate": "user_message_chunk",
                    "content": {"type": "text", "text": "earlier prompt"},
                }));
                self.say(&format!("replayed {SESSION_ID}"));
                reply(id, json!({}));
            }
            "session/prompt" => {
                if params["sessionId"].as_str() != self.session.as_deref() {
                    return reply_error(id, -32602, "unknown session");
                }
                let text: String = params["prompt"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                self.cancelled = false;
                self.prompt_id = id.clone();
                match self.script(&text) {
                    End::Stop(reason) => reply(id, json!({"stopReason": reason})),
                    End::Error(message) => reply_error(id, -32603, &message),
                }
            }
            _ => reply_error(id, -32601, &format!("fake agent does not handle {method}")),
        }
    }

    fn update(&self, update: Value) {
        self.update_for(SESSION_ID, update);
    }

    fn update_for(&self, session: &str, update: Value) {
        send(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session, "update": update},
        }));
    }

    fn say(&self, text: &str) {
        self.update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": text},
        }));
    }

    /// Sends a request to the client and waits for its answer, noting a
    /// `session/cancel` that arrives meanwhile.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = format!("fake-{}", self.next_id);
        self.next_id += 1;
        send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        while let Some(msg) = self.next() {
            match msg {
                Incoming::Response { id: rid, result } if rid == id => return result,
                Incoming::Response { .. } => {}
                Incoming::Notification { method, params } => self.note_cancel(&method, &params),
                Incoming::Request { id, .. } => reply_error(id, -32603, "busy with a turn"),
            }
        }
        // stdin closed: nobody is listening.
        std::process::exit(0);
    }

    fn note_cancel(&mut self, method: &str, params: &Value) {
        if method != "session/cancel" {
            return;
        }
        if params["sessionId"] == SESSION_ID {
            self.cancelled = true;
        } else {
            eprintln!("fake-acp-agent: session/cancel for the wrong session: {params}");
        }
    }

    fn path(&self, p: &str) -> PathBuf {
        let p = PathBuf::from(p);
        if p.is_absolute() { p } else { self.cwd.join(p) }
    }

    fn script(&mut self, text: &str) -> End {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
            match cmd {
                "say" => self.say(&unescape(rest)),
                "think" => self.update(json!({
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": unescape(rest)},
                })),
                "user" => self.update(json!({
                    "sessionUpdate": "user_message_chunk",
                    "content": {"type": "text", "text": unescape(rest)},
                })),
                "plan" => {
                    let entries: Vec<Value> = rest
                        .split('|')
                        .map(str::trim)
                        .map(|e| match e.strip_prefix('*') {
                            Some(done) => json!({"content": done, "priority": "medium", "status": "completed"}),
                            None => json!({"content": e, "priority": "medium", "status": "pending"}),
                        })
                        .collect();
                    self.update(json!({"sessionUpdate": "plan", "entries": entries}));
                }
                "tool" => {
                    let mut parts = rest.splitn(4, ' ');
                    let id = parts.next().unwrap_or("");
                    let kind = parts.next().unwrap_or("other");
                    let path = self.path(parts.next().unwrap_or(""));
                    let title = parts.next().unwrap_or(id);
                    self.update(json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": id,
                        "title": title,
                        "kind": kind,
                        "status": "pending",
                        "locations": [{"path": path}],
                    }));
                }
                "tool-update" => {
                    let (id, status) = rest.split_once(' ').unwrap_or((rest, "completed"));
                    self.update(json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": id,
                        "status": status,
                    }));
                }
                "write" => {
                    let (path, content) = rest.split_once(' ').unwrap_or((rest, ""));
                    let params = json!({
                        "sessionId": SESSION_ID,
                        "path": self.path(path),
                        "content": unescape(content),
                    });
                    match self.call("fs/write_text_file", params) {
                        Ok(_) => self.say(&format!("wrote {path}")),
                        Err(e) => self.say(&format!(
                            "write failed: {} {}",
                            e["code"],
                            e["message"].as_str().unwrap_or("")
                        )),
                    }
                }
                "read" => {
                    let mut parts = rest.split(' ');
                    let path = parts.next().unwrap_or("");
                    let mut params = json!({"sessionId": SESSION_ID, "path": self.path(path)});
                    if let Some(line) = parts.next().and_then(|v| v.parse::<u32>().ok()) {
                        params["line"] = json!(line);
                    }
                    if let Some(limit) = parts.next().and_then(|v| v.parse::<u32>().ok()) {
                        params["limit"] = json!(limit);
                    }
                    match self.call("fs/read_text_file", params) {
                        Ok(r) => match r["content"].as_str() {
                            Some(content) => self.say(&format!("read: {content}")),
                            None => self.say(&format!("read failed: malformed result {r}")),
                        },
                        Err(e) => self.say(&format!(
                            "read failed: {} {}",
                            e["code"],
                            e["message"].as_str().unwrap_or("")
                        )),
                    }
                }
                "ask" => {
                    let tool_id = format!("perm-{}", self.next_id);
                    let params = json!({
                        "sessionId": SESSION_ID,
                        "toolCall": {"toolCallId": tool_id, "title": rest},
                        "options": [
                            {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                            {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                        ],
                    });
                    match self.call("session/request_permission", params) {
                        Ok(r) => match (r["outcome"]["outcome"].as_str(), &r["outcome"]["optionId"]) {
                            (Some("selected"), Value::String(option)) => {
                                self.say(&format!("permission: {option}"))
                            }
                            (Some("cancelled"), _) => {
                                // Whether session/cancel arrived first.
                                if self.cancelled {
                                    self.say("permission: cancelled after session/cancel");
                                } else {
                                    self.say("permission: cancelled");
                                }
                                return End::Stop("cancelled".to_string());
                            }
                            _ => self.say(&format!("permission: malformed {r}")),
                        },
                        Err(e) => self.say(&format!("permission: error {e}")),
                    }
                }
                "write-raw" => {
                    let (path, content) = rest.split_once(' ').unwrap_or((rest, ""));
                    let params = json!({"sessionId": SESSION_ID, "path": path, "content": content});
                    match self.call("fs/write_text_file", params) {
                        Ok(_) => self.say(&format!("wrote {path}")),
                        Err(e) => self.say(&format!(
                            "write failed: {} {}",
                            e["code"],
                            e["message"].as_str().unwrap_or("")
                        )),
                    }
                }
                "ask-dup" => {
                    // Two permission requests with one id, before either
                    // is answered.
                    let id = format!("dup-{}", self.next_id);
                    self.next_id += 1;
                    for title in [rest, "second request"] {
                        send(&json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "method": "session/request_permission",
                            "params": {
                                "sessionId": SESSION_ID,
                                "toolCall": {"toolCallId": title, "title": title},
                                "options": [
                                    {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                                    {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                                ],
                            },
                        }));
                    }
                    let mut answers = 0;
                    while answers < 2 {
                        match self.next() {
                            Some(Incoming::Response { id: rid, result }) if rid == id => {
                                answers += 1;
                                match result {
                                    Ok(r) => self.say(&format!(
                                        "permission: {}",
                                        r["outcome"]["optionId"].as_str().unwrap_or("cancelled")
                                    )),
                                    Err(e) => self.say(&format!("duplicate: {}", e["code"])),
                                }
                            }
                            Some(Incoming::Notification { method, params }) => {
                                self.note_cancel(&method, &params)
                            }
                            Some(_) => {}
                            None => std::process::exit(0),
                        }
                    }
                }
                "fake-response" => {
                    // Shaped like an answer to the prompt being served, but
                    // without `jsonrpc` or `result`.
                    send(&json!({"id": self.prompt_id}));
                }
                "unversioned-write" => {
                    // A write request without `"jsonrpc": "2.0"`; nothing
                    // answers it, so do not wait.
                    send(&json!({
                        "id": "unversioned",
                        "method": "fs/write_text_file",
                        "params": {"sessionId": SESSION_ID, "path": self.path(rest), "content": "x"},
                    }));
                }
                "wait-cancel" => {
                    while !self.cancelled {
                        match self.next() {
                            Some(Incoming::Notification { method, params }) => {
                                self.note_cancel(&method, &params)
                            }
                            Some(Incoming::Request { id, .. }) => {
                                reply_error(id, -32603, "busy with a turn")
                            }
                            Some(_) => {}
                            None => std::process::exit(0),
                        }
                    }
                }
                "unknown-request" => match self.call("fake/unknown", json!({})) {
                    Ok(r) => self.say(&format!("unknown: ok {r}")),
                    Err(e) => self.say(&format!("unknown: {}", e["code"])),
                },
                "stderr" => eprintln!("{rest}"),
                "garbage" => send_raw("this is not json {"),
                "not-rpc" => send_raw("{\"hello\": 1}"),
                "update-unknown" => {
                    self.update(json!({"sessionUpdate": "available_commands_update", "availableCommands": []}));
                    self.update(json!({"sessionUpdate": "current_mode_update", "currentModeId": "code"}));
                    self.update(json!({"sessionUpdate": "made_up_kind", "x": 1}));
                }
                "bad-update" => self.update(json!({"sessionUpdate": "tool_call"})),
                "wrong-session" => self.update_for(
                    "other-session",
                    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "stray"}}),
                ),
                "caps" => self.say(&self.client_caps.to_string()),
                "cwd" => self.say(&self.cwd.to_string_lossy()),
                "spawn-child" | "orphan" => {
                    // A child that inherits stdout and stderr, like a tool
                    // process or a relaunched worker would.
                    let child = std::process::Command::new("sleep").arg("30").spawn();
                    match child {
                        Ok(c) => self.say(&format!("child {}", c.id())),
                        Err(e) => self.say(&format!("spawn failed: {e}")),
                    }
                    if cmd == "orphan" {
                        std::process::exit(4);
                    }
                }
                "crash" => {
                    eprintln!("fake-acp-agent: crashing on request");
                    std::process::exit(3);
                }
                "fail" => return End::Error("fake failure".to_string()),
                "stop" => return End::Stop(rest.to_string()),
                other => self.say(&format!("unknown command {other}")),
            }
            if self.cancelled {
                return End::Stop("cancelled".to_string());
            }
        }
        End::Stop("end_turn".to_string())
    }
}
