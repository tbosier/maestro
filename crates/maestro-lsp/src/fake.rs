//! A scriptable fake language server for tests. Not used by the editor.
//!
//! Test binaries run it by re-executing themselves with [`FAKE_ARG`]; their
//! `main` calls [`run_if_requested`] first. It speaks real framed JSON-RPC
//! over stdio and keeps its own copy of each document, applying incremental
//! changes with its own UTF-16 arithmetic (deliberately not shared with the
//! client's conversions, so tests compare two implementations).
//!
//! Behavior:
//!
//! * diagnostics: every whole word `ERROR`, `WARNING`, `INFO` or `HINT`
//!   becomes a diagnostic of that severity, published with the version;
//! * definition: the word at the position, found after `fn ` in the same
//!   document;
//! * references: every whole-word occurrence;
//! * hover: a markdown block naming the word;
//! * completion: after `.`, two methods (`len`, `push`, the second a
//!   snippet); elsewhere a fixed, complete list that covers each kind of
//!   edit (see [`completion`]). In a document containing `@incomplete` the
//!   list is marked incomplete, filtered here by the typed prefix, and has
//!   an extra item `<prefix>Echo`, so a client must ask again as the
//!   prefix grows;
//! * rename: every whole-word occurrence of the word at
//!   the position, in the document, in every other open document, and in
//!   every `.rs` file on disk next to the document that is not open. Open
//!   documents get their version, files on disk a `null` one. Markers in
//!   the requesting document bend the reply for failure tests: `@changes`
//!   uses the `changes` form, `@badversion` gives open documents a wrong
//!   version, `@overlap` and `@outofbounds` add a bad edit to the last
//!   file, `@createfile` adds a create-file operation. The new name
//!   `error` gets an error reply. `prepareRename` answers the word's range
//!   and the word as placeholder, `null` off a word, and an error on the
//!   word `builtin`;
//! * formatting and range formatting remove trailing spaces and tabs, one
//!   edit per line (only lines the range touches for range formatting).
//!   `@formatwhole` answers one edit replacing the whole text instead;
//!   `@formaterror` an error;
//! * code actions: a quick fix replacing the range
//!   of each `found ERROR` diagnostic the request sends with `OK`, plus a
//!   command run after it; an action without an edit that
//!   `codeAction/resolve` fills in (uppercase the word); a bare command
//!   that makes the server send `workspace/applyEdit` (inserting a header
//!   line, or at line 9999 in a document containing `@badheader`); a
//!   command that fails; a disabled action;
//! * signature help: inside the parentheses of a call to a function
//!   defined in the same document as `fn name(...)`, its signature with
//!   parameter offsets and the parameter the cursor is in; `null`
//!   elsewhere. Triggered by `(` and `,`;
//! * a document containing `@slow` delays every `textDocument/` request
//!   about it, `codeAction/resolve`, and the `workspace/applyEdit` of a
//!   `fake.applyEdit` command, by [`SLOW_MS`]; one containing
//!   `@crash` makes the server exit with code 3 as soon as it sees it;
//!   `@orphan` makes it start a `sleep` child that keeps stdout open and
//!   exit with code 4;
//! * `fake/documentText` returns its copy of a document and any protocol
//!   errors it noticed; `fake/stats` returns the notifications it received,
//!   the completion requests it answered, the commands it ran (with the
//!   text of every open document then, as `openTexts`), the
//!   responses to its own requests and the `initialize` params.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};

use serde_json::{Value, json};

use crate::framing::{read_message, write_message};
use crate::jsonrpc::{self, Message};

pub const FAKE_ARG: &str = "--maestro-fake-lsp";
pub const SLOW_MS: u64 = 600;
/// Extra argument: wait 400 ms before answering `initialize`.
pub const SLOW_INIT_ARG: &str = "--fake-slow-init";
/// Extra argument: ask for the text in `didSave`.
pub const SAVE_TEXT_ARG: &str = "--fake-save-text";
/// Extra argument: do not offer `prepareRename`.
pub const NO_PREPARE_RENAME_ARG: &str = "--fake-no-prepare-rename";

/// If this process was started as the fake server, serves stdio and exits.
pub fn run_if_requested() {
    if std::env::args().any(|a| a == FAKE_ARG) {
        let code = serve();
        std::process::exit(code);
    }
}

struct Doc {
    text: String,
    version: i64,
}

#[derive(Default)]
struct State {
    docs: HashMap<String, Doc>,
    errors: Vec<String>,
    received: Vec<Value>,
    /// Completion requests, with their params.
    requests: Vec<Value>,
    /// `workspace/executeCommand` params.
    commands: Vec<Value>,
    /// Responses to the server's own requests.
    responses: Vec<Value>,
    initialize: Value,
    next_request: u64,
}

fn serve() -> i32 {
    eprintln!("fake-lsp: started");
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let mut st = State::default();
    loop {
        let msg = match read_message(&mut input) {
            Ok(Some(v)) => v,
            Ok(None) => return 0,
            Err(e) => {
                eprintln!("fake-lsp: bad input: {e}");
                return 2;
            }
        };
        let Some(msg) = Message::parse(msg) else {
            continue;
        };
        match msg {
            Message::Request { id, method, params } => {
                let reply = match handle_request(&mut st, &method, &params, &mut out) {
                    Ok(v) => jsonrpc::response(id, v),
                    Err((code, msg)) => jsonrpc::error_response(id, code, &msg),
                };
                let _ = write_message(&mut out, &reply);
            }
            Message::Notification { method, params } => {
                if method == "exit" {
                    let _ = out.flush();
                    return 0;
                }
                st.received
                    .push(json!({"method": method, "params": params}));
                let uri = params["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                match method.as_str() {
                    "initialized" => {
                        let _ = write_message(
                            &mut out,
                            &jsonrpc::notification(
                                "window/logMessage",
                                json!({"type": 3, "message": "fake-lsp: initialized"}),
                            ),
                        );
                    }
                    "textDocument/didOpen" => {
                        let td = &params["textDocument"];
                        st.docs.insert(
                            uri.clone(),
                            Doc {
                                text: td["text"].as_str().unwrap_or("").to_string(),
                                version: td["version"].as_i64().unwrap_or(0),
                            },
                        );
                        after_change(&mut st, &uri, &mut out);
                    }
                    "textDocument/didChange" => {
                        let version = params["textDocument"]["version"].as_i64().unwrap_or(-1);
                        let Some(doc) = st.docs.get_mut(&uri) else {
                            st.errors.push(format!("didChange for unopened {uri}"));
                            continue;
                        };
                        if version <= doc.version {
                            st.errors
                                .push(format!("version went from {} to {version}", doc.version));
                        }
                        doc.version = version;
                        let mut errors = Vec::new();
                        for change in params["contentChanges"].as_array().into_iter().flatten() {
                            if let Err(e) = apply_change(&mut doc.text, change) {
                                errors.push(e);
                            }
                        }
                        st.errors.extend(errors);
                        after_change(&mut st, &uri, &mut out);
                    }
                    "textDocument/didClose" => {
                        st.docs.remove(&uri);
                    }
                    _ => {}
                }
            }
            Message::Response { id, result } => {
                st.responses.push(match result {
                    Ok(v) => json!({"id": id, "result": v}),
                    Err(e) => json!({"id": id, "error": e.message}),
                });
            }
        }
    }
}

fn after_change(st: &mut State, uri: &str, out: &mut impl Write) {
    let Some(doc) = st.docs.get(uri) else {
        return;
    };
    if doc.text.contains("@crash") {
        eprintln!("fake-lsp: crashing on request");
        let _ = out.flush();
        std::process::exit(3);
    }
    if doc.text.contains("@orphan") {
        // Exit while a child keeps our stdout and stderr open.
        let _ = std::process::Command::new("sleep").arg("5").spawn();
        let _ = out.flush();
        std::process::exit(4);
    }
    let mut diags = Vec::new();
    for (word, severity) in [("ERROR", 1), ("WARNING", 2), ("INFO", 3), ("HINT", 4)] {
        for start in word_occurrences(&doc.text, word) {
            diags.push(json!({
                "range": {
                    "start": position(&doc.text, start),
                    "end": position(&doc.text, start + word.len()),
                },
                "severity": severity,
                "source": "fake",
                "message": format!("found {word}"),
            }));
        }
    }
    let _ = write_message(
        out,
        &jsonrpc::notification(
            "textDocument/publishDiagnostics",
            json!({"uri": uri, "version": doc.version, "diagnostics": diags}),
        ),
    );
}

/// An error reply: (code, message).
type Failure = (i64, String);

fn fail(message: &str) -> Failure {
    (-32603, message.to_string())
}

fn handle_request(
    st: &mut State,
    method: &str,
    params: &Value,
    out: &mut impl Write,
) -> Result<Value, Failure> {
    let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
    let slow = st.docs.get(uri).is_some_and(|d| d.text.contains("@slow"));
    if method.starts_with("textDocument/") && slow {
        std::thread::sleep(std::time::Duration::from_millis(SLOW_MS));
    }
    let doc = st.docs.get(uri);
    Ok(match method {
        "initialize" => {
            st.initialize = params.clone();
            if std::env::args().any(|a| a == SLOW_INIT_ARG) {
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
            let include_text = std::env::args().any(|a| a == SAVE_TEXT_ARG);
            let rename = if std::env::args().any(|a| a == NO_PREPARE_RENAME_ARG) {
                json!(true)
            } else {
                json!({"prepareProvider": true})
            };
            json!({
                "capabilities": {
                    "positionEncoding": "utf-16",
                    "textDocumentSync": {
                        "openClose": true,
                        "change": 2,
                        "save": {"includeText": include_text},
                    },
                    "definitionProvider": true,
                    "referencesProvider": true,
                    "hoverProvider": true,
                    "completionProvider": {"triggerCharacters": ["."]},
                    "renameProvider": rename,
                    "documentFormattingProvider": true,
                    "documentRangeFormattingProvider": true,
                    "codeActionProvider": {"resolveProvider": true},
                    "executeCommandProvider": {
                        "commands": ["fake.applyEdit", "fake.record", "fake.fail"],
                    },
                    "signatureHelpProvider": {"triggerCharacters": ["(", ","]},
                },
                "serverInfo": {"name": "fake-lsp"},
            })
        }
        "shutdown" => Value::Null,
        "fake/documentText" => match doc {
            Some(d) => json!({"text": d.text, "version": d.version, "errors": st.errors}),
            None => json!({"text": null, "errors": st.errors}),
        },
        "fake/stats" => json!({
            "received": st.received,
            "requests": st.requests,
            "commands": st.commands,
            "responses": st.responses,
            "initialize": st.initialize,
        }),
        "textDocument/prepareRename" => {
            let Some(doc) = doc else {
                return Ok(Value::Null);
            };
            let at = offset(&doc.text, &params["position"]).ok_or(fail("bad position"))?;
            let (start, end) = word_range(&doc.text, at);
            match &doc.text[start..end] {
                "" => Value::Null,
                "builtin" => return Err(fail("cannot rename a builtin")),
                word => json!({
                    "range": range_json(&doc.text, start, end),
                    "placeholder": word,
                }),
            }
        }
        "textDocument/rename" => rename(st, uri, params)?,
        "textDocument/formatting" | "textDocument/rangeFormatting" => {
            let Some(doc) = doc else {
                return Ok(Value::Null);
            };
            if doc.text.contains("@formaterror") {
                return Err(fail("the fake formatter failed"));
            }
            // A range ending at the start of a line does not touch it.
            let lines = (method == "textDocument/rangeFormatting").then(|| {
                let at = |p: &str, k: &str| params["range"][p][k].as_u64().unwrap_or(0) as usize;
                let (first, mut last) = (at("start", "line"), at("end", "line"));
                if last > first && at("end", "character") == 0 {
                    last -= 1;
                }
                (first, last)
            });
            format(&doc.text, lines)
        }
        "textDocument/codeAction" => match doc {
            Some(doc) => code_actions(uri, doc, params),
            None => Value::Null,
        },
        "codeAction/resolve" => {
            let data = &params["data"];
            let uri = data["uri"].as_str().unwrap_or("");
            let doc = st.docs.get(uri).ok_or(fail("resolve: document not open"))?;
            if doc.text.contains("@slow") {
                std::thread::sleep(std::time::Duration::from_millis(SLOW_MS));
            }
            let at = offset(&doc.text, &data["position"]).ok_or(fail("bad position"))?;
            let (start, end) = word_range(&doc.text, at);
            let mut action = params.clone();
            action["edit"] = json!({"documentChanges": [{
                "textDocument": {"uri": uri, "version": doc.version},
                "edits": [{
                    "range": range_json(&doc.text, start, end),
                    "newText": doc.text[start..end].to_uppercase(),
                }],
            }]});
            action
        }
        "workspace/executeCommand" => {
            // With the text of every open document as the command saw it.
            let mut texts: Vec<&str> = st.docs.values().map(|d| d.text.as_str()).collect();
            texts.sort_unstable();
            let mut record = params.clone();
            record["openTexts"] = json!(texts);
            st.commands.push(record);
            match params["command"].as_str() {
                Some("fake.applyEdit") => {
                    if st.docs.values().any(|d| d.text.contains("@slow")) {
                        std::thread::sleep(std::time::Duration::from_millis(SLOW_MS));
                    }
                    st.next_request += 1;
                    let request = json!({
                        "jsonrpc": "2.0",
                        "id": format!("fake-{}", st.next_request),
                        "method": "workspace/applyEdit",
                        "params": {"label": "fake edit", "edit": params["arguments"][0]},
                    });
                    let _ = write_message(out, &request);
                    Value::Null
                }
                Some("fake.fail") => return Err(fail("the fake command failed")),
                _ => Value::Null,
            }
        }
        "textDocument/signatureHelp" => match doc {
            Some(doc) => offset(&doc.text, &params["position"])
                .map_or(Value::Null, |at| signature_help(&doc.text, at)),
            None => Value::Null,
        },
        "textDocument/completion" => {
            st.requests.push(params.clone());
            let doc = st.docs.get(uri);
            match doc.and_then(|d| Some((d, offset(&d.text, &params["position"])?))) {
                Some((d, at)) => completion(&d.text, at),
                None => Value::Null,
            }
        }
        "textDocument/definition" | "textDocument/references" | "textDocument/hover" => {
            let Some(doc) = doc else {
                return Ok(Value::Null);
            };
            let Some(at) = offset(&doc.text, &params["position"]) else {
                return Ok(Value::Null);
            };
            let word = word_at(&doc.text, at);
            if word.is_empty() {
                return Ok(Value::Null);
            }
            let loc = |start: usize| {
                json!({
                    "uri": uri,
                    "range": {
                        "start": position(&doc.text, start),
                        "end": position(&doc.text, start + word.len()),
                    },
                })
            };
            match method {
                "textDocument/definition" => {
                    let needle = format!("fn {word}");
                    word_occurrences(&doc.text, &needle)
                        .first()
                        .map(|&s| json!([loc(s + 3)]))
                        .unwrap_or(Value::Null)
                }
                "textDocument/references" => Value::Array(
                    word_occurrences(&doc.text, word)
                        .into_iter()
                        .map(loc)
                        .collect(),
                ),
                _ => json!({
                    "contents": {
                        "kind": "markdown",
                        "value": format!("```rust\nfn {word}()\n```\n\nDocumentation for `{word}`."),
                    },
                }),
            }
        }
        _ => Value::Null,
    })
}

fn range_json(text: &str, start: usize, end: usize) -> Value {
    json!({"start": position(text, start), "end": position(text, end)})
}

/// A rename reply. See the module documentation for the markers.
fn rename(st: &State, uri: &str, params: &Value) -> Result<Value, Failure> {
    let doc = st.docs.get(uri).ok_or(fail("rename: document not open"))?;
    let at = offset(&doc.text, &params["position"]).ok_or(fail("bad position"))?;
    let word = word_at(&doc.text, at).to_string();
    if word.is_empty() {
        return Err((-32602, "nothing to rename here".into()));
    }
    let new = params["newName"].as_str().unwrap_or_default();
    if new == "error" {
        return Err(fail("the fake rename failed"));
    }
    let marker = |m: &str| doc.text.contains(m);
    let bump = if marker("@badversion") { 7 } else { 0 };
    // (uri, text, version): this document, other open ones, then files on
    // disk next to it.
    let mut files: Vec<(String, String, Option<i64>)> =
        vec![(uri.to_string(), doc.text.clone(), Some(doc.version + bump))];
    let mut others: Vec<_> = st.docs.iter().filter(|(u, _)| *u != uri).collect();
    others.sort_by(|a, b| a.0.cmp(b.0));
    for (u, d) in others {
        files.push((u.clone(), d.text.clone(), Some(d.version + bump)));
    }
    let dir = crate::convert::uri_to_path(uri).and_then(|p| Some(p.parent()?.to_path_buf()));
    if let Some(dir) = &dir {
        let mut disk: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "rs"))
            .collect();
        disk.sort();
        for p in disk {
            let u = crate::convert::path_to_uri(&p);
            if st.docs.contains_key(&u) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p) {
                files.push((u, text, None));
            }
        }
    }
    let mut changes: Vec<(String, Option<i64>, Vec<Value>)> = files
        .into_iter()
        .filter_map(|(u, text, version)| {
            let edits: Vec<Value> = word_occurrences(&text, &word)
                .into_iter()
                .map(|s| json!({"range": range_json(&text, s, s + word.len()), "newText": new}))
                .collect();
            (!edits.is_empty()).then_some((u, version, edits))
        })
        .collect();
    if let Some((_, _, edits)) = changes.last_mut() {
        if marker("@overlap") {
            // Starts where the first edit starts and ends inside it.
            let mut bad = edits[0].clone();
            let start = bad["range"]["start"]["character"].as_u64().unwrap_or(0);
            bad["range"]["end"] = bad["range"]["start"].clone();
            bad["range"]["end"]["character"] = json!(start + 1);
            bad["newText"] = json!("overlap");
            edits.push(bad);
        }
        if marker("@outofbounds") {
            let far = json!({"line": 9999, "character": 0});
            edits.push(json!({"range": {"start": far, "end": far}, "newText": "far"}));
        }
    }
    if marker("@changes") {
        let map: serde_json::Map<String, Value> = changes
            .into_iter()
            .map(|(u, _, edits)| (u, Value::Array(edits)))
            .collect();
        return Ok(json!({"changes": map}));
    }
    let mut document_changes: Vec<Value> = changes
        .into_iter()
        .map(|(u, version, edits)| {
            json!({"textDocument": {"uri": u, "version": version}, "edits": edits})
        })
        .collect();
    if marker("@createfile")
        && let Some(dir) = &dir
    {
        let new_file = crate::convert::path_to_uri(&dir.join("created.rs"));
        document_changes.push(json!({"kind": "create", "uri": new_file}));
    }
    Ok(json!({"documentChanges": document_changes}))
}

/// Formatting edits: trailing spaces and tabs removed, one edit per line,
/// for lines `first..=last` when given.
fn format(text: &str, lines: Option<(usize, usize)>) -> Value {
    let mut edits = Vec::new();
    let mut formatted = String::new();
    let mut start = 0;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let body = line.trim_end_matches(['\n', '\r']);
        let kept = body.trim_end_matches([' ', '\t']);
        let in_range = lines.is_none_or(|(a, b)| a <= i && i <= b);
        if in_range && kept.len() < body.len() {
            edits.push(json!({
                "range": range_json(text, start + kept.len(), start + body.len()),
                "newText": "",
            }));
            formatted.push_str(kept);
            formatted.push_str(&line[body.len()..]);
        } else {
            formatted.push_str(line);
        }
        start += line.len();
    }
    if text.contains("@formatwhole") {
        return json!([{"range": range_json(text, 0, text.len()), "newText": formatted}]);
    }
    Value::Array(edits)
}

/// Code actions for a request. See the module documentation.
fn code_actions(uri: &str, doc: &Doc, params: &Value) -> Value {
    let mut actions = Vec::new();
    for d in params["context"]["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if d["message"] == "found ERROR" {
            actions.push(json!({
                "title": "Replace ERROR with OK",
                "kind": "quickfix",
                "isPreferred": true,
                "diagnostics": [d],
                "edit": {"documentChanges": [{
                    "textDocument": {"uri": uri, "version": doc.version},
                    "edits": [{"range": d["range"], "newText": "OK"}],
                }]},
                "command": {"title": "record", "command": "fake.record", "arguments": ["quickfix"]},
            }));
        }
    }
    let line = if doc.text.contains("@badheader") {
        9999
    } else {
        0
    };
    let origin = json!({"line": line, "character": 0});
    actions.push(json!({
        "title": "Uppercase the word",
        "kind": "refactor.rewrite",
        "data": {"uri": uri, "position": params["range"]["start"]},
    }));
    actions.push(json!({
        "title": "Insert a header comment",
        "command": "fake.applyEdit",
        "arguments": [{"changes": {uri: [
            {"range": {"start": origin, "end": origin}, "newText": "// header\n"},
        ]}}],
    }));
    actions.push(json!({"title": "Run a failing command", "command": "fake.fail"}));
    actions.push(json!({
        "title": "Unavailable action",
        "kind": "refactor",
        "disabled": {"reason": "not available here"},
    }));
    Value::Array(actions)
}

/// Signature help for a cursor at byte `at`: the innermost unclosed call
/// before it, if its function is defined in `text`.
fn signature_help(text: &str, at: usize) -> Value {
    let mut depth = 0;
    let mut open = None;
    for (i, c) in text[..at].char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' if depth == 0 => {
                open = Some(i);
                break;
            }
            '(' => depth -= 1,
            _ => {}
        }
    }
    let Some(open) = open else {
        return Value::Null;
    };
    let (name_start, _) = word_range(text, open);
    let name = &text[name_start..open];
    if name.is_empty() {
        return Value::Null;
    }
    let Some(def) = text.find(&format!("fn {name}(")) else {
        return Value::Null;
    };
    let params_start = def + 4 + name.len();
    let Some(len) = text[params_start..].find(')') else {
        return Value::Null;
    };
    let params_text = &text[params_start..params_start + len];
    let label = format!("fn {name}({params_text})");
    let units = |byte: usize| label[..byte].encode_utf16().count();
    let mut parameters = Vec::new();
    let mut at_byte = 4 + name.len();
    for part in params_text.split(',') {
        let lead = part.len() - part.trim_start().len();
        let start = at_byte + lead;
        let end = start + part.trim().len();
        parameters.push(json!({"label": [units(start), units(end)]}));
        at_byte += part.len() + 1;
    }
    let mut active = 0;
    let mut depth = 0;
    for c in text[open + 1..at].chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => active += 1,
            _ => {}
        }
    }
    json!({
        "signatures": [{"label": label, "parameters": parameters}],
        "activeSignature": 0,
        "activeParameter": active,
    })
}

/// The completion list for a cursor at byte `at`. Ranges are computed
/// here, in UTF-16, independently of the client.
pub fn completion(text: &str, at: usize) -> Value {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let start = text[..at]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_word(c))
        .last()
        .map_or(at, |(i, _)| i);
    let end = text[at..]
        .char_indices()
        .find(|&(_, c)| !is_word(c))
        .map_or(text.len(), |(i, _)| at + i);
    let prefix = &text[start..at];
    let range = |a: usize, b: usize| json!({"start": position(text, a), "end": position(text, b)});
    let edit = |new: &str| json!({"range": range(start, at), "newText": new});
    if text[..start].ends_with('.') {
        return json!({"isIncomplete": false, "items": [
            {"label": "len", "kind": 2, "detail": "fn(&self) -> usize", "textEdit": edit("len()")},
            {"label": "push", "kind": 2, "detail": "fn(&mut self, value: T)",
             "insertTextFormat": 2, "textEdit": edit("push(${1:value})")},
        ]});
    }
    let mut items = vec![
        json!({"label": "collections", "kind": 9, "detail": "std::collections", "sortText": "1",
               "textEdit": edit("collections")}),
        json!({"label": "column", "kind": 3, "detail": "fn() -> u32", "sortText": "2",
               "textEdit": {"insert": range(start, at), "replace": range(start, end), "newText": "column"}}),
        json!({"label": "Color", "kind": 22, "detail": "struct Color", "sortText": "3",
               "insertText": "Color"}),
        json!({"label": "col_label_only", "kind": 6, "sortText": "4"}),
        json!({"label": "HashMap", "kind": 22, "detail": "std::collections::HashMap", "sortText": "5",
               "textEdit": edit("HashMap"),
               "additionalTextEdits": [{"range": range(0, 0), "newText": "use std::collections::HashMap;\n"}]}),
        json!({"label": "configure", "kind": 3, "detail": "fn(value: u32, x: &str)", "sortText": "6",
               "insertTextFormat": 2, "textEdit": edit("configure(${1:value}, ${2:\\$x})$0;")}),
        json!({"label": "café_crème", "kind": 6, "detail": "café", "sortText": "7",
               "textEdit": edit("café_crème")}),
        json!({"label": "日本語", "kind": 21, "sortText": "8", "textEdit": edit("日本語")}),
    ];
    let incomplete = text.contains("@incomplete");
    if incomplete {
        let lower = prefix.to_lowercase();
        items.retain(|i| {
            i["label"]
                .as_str()
                .is_some_and(|l| l.to_lowercase().starts_with(&lower))
        });
        items.push(
            json!({"label": format!("{prefix}Echo"), "kind": 1, "sortText": "0",
                          "textEdit": edit(&format!("{prefix}Echo"))}),
        );
    }
    json!({"isIncomplete": incomplete, "items": items})
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Byte offsets of whole-word occurrences of `needle`.
fn word_occurrences(text: &str, needle: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    text.match_indices(needle)
        .map(|(i, _)| i)
        .filter(|&i| {
            let before = i == 0 || !is_word_byte(bytes[i - 1]);
            let end = i + needle.len();
            let after = end >= bytes.len() || !is_word_byte(bytes[end]);
            before && after
        })
        .collect()
}

fn word_at(text: &str, at: usize) -> &str {
    let (start, end) = word_range(text, at);
    &text[start..end]
}

/// Byte range of the word around byte `at`.
fn word_range(text: &str, at: usize) -> (usize, usize) {
    let bytes = text.as_bytes();
    let mut start = at.min(bytes.len());
    while start > 0 && is_word_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = at.min(bytes.len());
    while end < bytes.len() && is_word_byte(bytes[end]) {
        end += 1;
    }
    (start, end)
}

/// Byte offset of each line start. Lines end at `\n`, `\r\n` or `\r`.
fn line_starts(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut starts = vec![0];
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => starts.push(i + 1),
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => {
                starts.push(i + 2);
                i += 1;
            }
            b'\r' => starts.push(i + 1),
            _ => {}
        }
        i += 1;
    }
    starts
}

/// Converts a protocol position to a byte offset, clamping the character
/// to the line's end as the protocol requires.
fn offset(text: &str, pos: &Value) -> Option<usize> {
    let line = usize::try_from(pos["line"].as_u64()?).ok()?;
    let character = usize::try_from(pos["character"].as_u64()?).ok()?;
    let starts = line_starts(text);
    let start = *starts.get(line)?;
    let mut units = 0;
    for (i, c) in text[start..].char_indices() {
        if c == '\n' || c == '\r' || units >= character {
            return Some(start + i);
        }
        units += c.len_utf16();
    }
    Some(text.len())
}

fn position(text: &str, byte: usize) -> Value {
    let starts = line_starts(text);
    let line = starts.partition_point(|&s| s <= byte) - 1;
    let character: usize = text[starts[line]..byte].chars().map(char::len_utf16).sum();
    json!({"line": line, "character": character})
}

/// Applies one `TextDocumentContentChangeEvent`.
pub fn apply_change(text: &mut String, change: &Value) -> Result<(), String> {
    let new = change["text"].as_str().ok_or("change without text")?;
    let Some(range) = change.get("range") else {
        *text = new.to_string();
        return Ok(());
    };
    let start = offset(text, &range["start"]).ok_or(format!("bad start {range}"))?;
    let end = offset(text, &range["end"]).ok_or(format!("bad end {range}"))?;
    if start > end {
        return Err(format!("reversed range {range}"));
    }
    text.replace_range(start..end, new);
    Ok(())
}

/// A minimal test runner for `harness = false` test binaries, which need
/// their own `main` so they can double as the fake server. Runs each test,
/// catching panics, and exits non-zero if any failed. A plain argument
/// filters tests by substring, like libtest.
pub fn run_tests(tests: &[(&str, fn())]) {
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let selected: Vec<_> = tests
        .iter()
        .filter(|(name, _)| filter.as_deref().is_none_or(|f| name.contains(f)))
        .collect();
    println!("\nrunning {} tests", selected.len());
    let mut failed = Vec::new();
    for (name, test) in &selected {
        let ok = std::panic::catch_unwind(test).is_ok();
        println!("test {name} ... {}", if ok { "ok" } else { "FAILED" });
        if !ok {
            failed.push(*name);
        }
    }
    let passed = selected.len() - failed.len();
    println!(
        "\ntest result: {}. {passed} passed; {} failed\n",
        if failed.is_empty() { "ok" } else { "FAILED" },
        failed.len()
    );
    if !failed.is_empty() {
        std::process::exit(101);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets_and_positions_agree() {
        let text = "a😀b\r\nxy\rz";
        assert_eq!(offset(text, &json!({"line": 0, "character": 3})), Some(5));
        assert_eq!(offset(text, &json!({"line": 0, "character": 99})), Some(6));
        assert_eq!(offset(text, &json!({"line": 1, "character": 1})), Some(9));
        assert_eq!(offset(text, &json!({"line": 2, "character": 0})), Some(11));
        assert_eq!(offset(text, &json!({"line": 3, "character": 0})), None);
        assert_eq!(position(text, 5), json!({"line": 0, "character": 3}));
        assert_eq!(position(text, 11), json!({"line": 2, "character": 0}));
        let mut s = text.to_string();
        apply_change(
            &mut s,
            &json!({"range": {"start": {"line": 0, "character": 1}, "end": {"line": 1, "character": 1}}, "text": "-"}),
        )
        .unwrap();
        assert_eq!(s, "a-y\rz");
    }
}
