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
//! * a document containing `@slow` delays definition, references and hover
//!   by [`SLOW_MS`]; one containing `@crash` makes the server exit with
//!   code 3 as soon as it sees it; `@orphan` makes it start a `sleep`
//!   child that keeps stdout open and exit with code 4;
//! * `fake/documentText` returns its copy of a document and any protocol
//!   errors it noticed; `fake/stats` returns the notifications it received.

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
                let reply = handle_request(&mut st, &method, &params);
                let _ = write_message(&mut out, &jsonrpc::response(id, reply));
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
            Message::Response { .. } => {}
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

fn handle_request(st: &mut State, method: &str, params: &Value) -> Value {
    let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
    let doc = st.docs.get(uri);
    if matches!(
        method,
        "textDocument/definition" | "textDocument/references" | "textDocument/hover"
    ) && doc.is_some_and(|d| d.text.contains("@slow"))
    {
        std::thread::sleep(std::time::Duration::from_millis(SLOW_MS));
    }
    match method {
        "initialize" => {
            if std::env::args().any(|a| a == SLOW_INIT_ARG) {
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
            let include_text = std::env::args().any(|a| a == SAVE_TEXT_ARG);
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
                },
                "serverInfo": {"name": "fake-lsp"},
            })
        }
        "shutdown" => Value::Null,
        "fake/documentText" => match doc {
            Some(d) => json!({"text": d.text, "version": d.version, "errors": st.errors}),
            None => json!({"text": null, "errors": st.errors}),
        },
        "fake/stats" => json!({"received": st.received}),
        "textDocument/definition" | "textDocument/references" | "textDocument/hover" => {
            let Some(doc) = doc else {
                return Value::Null;
            };
            let Some(at) = offset(&doc.text, &params["position"]) else {
                return Value::Null;
            };
            let word = word_at(&doc.text, at);
            if word.is_empty() {
                return Value::Null;
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
    }
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
    let bytes = text.as_bytes();
    let mut start = at.min(bytes.len());
    while start > 0 && is_word_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = at.min(bytes.len());
    while end < bytes.len() && is_word_byte(bytes[end]) {
        end += 1;
    }
    &text[start..end]
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
