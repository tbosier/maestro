//! Client tests against the fake server, run as a real child process.
//!
//! `harness = false`: this binary is also the fake server. When started
//! with `--maestro-fake-lsp` it serves stdio instead of running tests.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use maestro_core::{Change, ChangeSet, Rope};
use maestro_lsp::client::recv_reply;
use maestro_lsp::convert::content_changes;
use maestro_lsp::fake::{self, FAKE_ARG};
use maestro_lsp::{Client, Event, StartOptions, Value, json};

fn main() {
    fake::run_if_requested();
    fake::run_tests(&[
        (
            "initialize_and_capture_stderr",
            initialize_and_capture_stderr,
        ),
        (
            "messages_sent_before_initialize_arrive_in_order",
            messages_sent_before_initialize_arrive_in_order,
        ),
        (
            "diagnostics_use_utf16_positions",
            diagnostics_use_utf16_positions,
        ),
        (
            "cancel_disconnects_and_notifies",
            cancel_disconnects_and_notifies,
        ),
        (
            "crash_is_reported_and_requests_fail_fast",
            crash_is_reported_and_requests_fail_fast,
        ),
        (
            "incremental_changes_match_random_edits",
            incremental_changes_match_random_edits,
        ),
        ("missing_program_is_an_error", missing_program_is_an_error),
        (
            "exit_is_seen_while_a_child_holds_stdout",
            exit_is_seen_while_a_child_holds_stdout,
        ),
    ]);
}

const WAIT: Duration = Duration::from_secs(10);

fn start() -> (Client, mpsc::Receiver<Value>, mpsc::Receiver<Event>) {
    start_with(&[])
}

fn start_with(extra: &[&str]) -> (Client, mpsc::Receiver<Value>, mpsc::Receiver<Event>) {
    let (tx, events) = mpsc::channel();
    let mut args = vec![FAKE_ARG.to_string()];
    args.extend(extra.iter().map(|a| a.to_string()));
    let opts = StartOptions {
        program: std::env::current_exe().unwrap(),
        args,
        root: std::env::temp_dir(),
    };
    let (client, init) = Client::start(opts, Box::new(move |e| drop(tx.send(e)))).unwrap();
    let (itx, irx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Some(Ok(v)) = recv_reply(&init, WAIT) {
            let _ = itx.send(v);
        }
    });
    (client, irx, events)
}

fn next_notification(events: &mpsc::Receiver<Event>, method: &str) -> Value {
    let deadline = Instant::now() + WAIT;
    while let Ok(e) = events.recv_timeout(deadline - Instant::now()) {
        if let Event::Notification { method: m, params } = e
            && m == method
        {
            return params;
        }
    }
    panic!("no {method} notification");
}

fn open(client: &Client, uri: &str, text: &str) {
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri, "languageId": "rust", "version": 0, "text": text}}),
    );
}

fn server_text(client: &Client, uri: &str) -> Value {
    let (_, rx) = client.request("fake/documentText", json!({"textDocument": {"uri": uri}}));
    recv_reply(&rx, WAIT).expect("reply").expect("ok")
}

fn initialize_and_capture_stderr() {
    let (client, init, _events) = start();
    let result = init.recv_timeout(WAIT).expect("initialize result");
    assert_eq!(result["serverInfo"]["name"], "fake-lsp");
    assert_eq!(client.capabilities().unwrap()["hoverProvider"], true);
    // stderr and window/logMessage land in the log, not on our terminal.
    let deadline = Instant::now() + WAIT;
    loop {
        let log = client.log();
        if log.iter().any(|l| l == "fake-lsp: started")
            && log.iter().any(|l| l == "fake-lsp: initialized")
        {
            break;
        }
        assert!(Instant::now() < deadline, "log: {log:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn messages_sent_before_initialize_arrive_in_order() {
    // The server waits 400 ms before answering initialize, so this didOpen
    // is certainly sent before the reply and must wait in the backlog.
    let (client, init, events) = start_with(&[fake::SLOW_INIT_ARG]);
    open(&client, "file:///a.rs", "ERROR\n");
    assert!(client.capabilities().is_none(), "not initialized yet");
    init.recv_timeout(WAIT).expect("initialize result");
    let d = next_notification(&events, "textDocument/publishDiagnostics");
    assert_eq!(d["uri"], "file:///a.rs");
    let (_, rx) = client.request("fake/stats", Value::Null);
    let stats = recv_reply(&rx, WAIT).unwrap().unwrap();
    let methods: Vec<&str> = stats["received"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["method"].as_str().unwrap())
        .collect();
    assert_eq!(methods, ["initialized", "textDocument/didOpen"]);
}

fn diagnostics_use_utf16_positions() {
    let (client, _init, events) = start();
    open(&client, "file:///u.rs", "😀😀 ERROR\n");
    let d = next_notification(&events, "textDocument/publishDiagnostics");
    let diag = &d["diagnostics"][0];
    assert_eq!(diag["severity"], 1);
    // Two astral chars are four UTF-16 units, plus a space.
    assert_eq!(diag["range"]["start"], json!({"line": 0, "character": 5}));
    assert_eq!(diag["range"]["end"], json!({"line": 0, "character": 10}));
    let text = Rope::from_str("😀😀 ERROR\n");
    assert_eq!(
        maestro_lsp::convert::range_to_chars(&text, &diag["range"]),
        Some((3, 8))
    );
}

fn cancel_disconnects_and_notifies() {
    let (client, _init, events) = start();
    open(&client, "file:///s.rs", "@slow fn f() {}\n");
    next_notification(&events, "textDocument/publishDiagnostics");
    let (id, rx) = client.request(
        "textDocument/hover",
        json!({"textDocument": {"uri": "file:///s.rs"}, "position": {"line": 0, "character": 10}}),
    );
    let t = Instant::now();
    client.cancel(id);
    assert!(
        rx.recv_timeout(WAIT).is_err(),
        "a cancelled request yields nothing"
    );
    assert!(
        t.elapsed() < Duration::from_millis(fake::SLOW_MS),
        "cancel did not wait"
    );
    let (_, rx) = client.request("fake/stats", Value::Null);
    let stats = recv_reply(&rx, WAIT).unwrap().unwrap();
    assert!(
        stats["received"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["method"] == "$/cancelRequest" && m["params"]["id"] == id),
        "{stats}"
    );
}

fn crash_is_reported_and_requests_fail_fast() {
    let (client, _init, events) = start();
    open(&client, "file:///c.rs", "fine\n");
    next_notification(&events, "textDocument/publishDiagnostics");
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": "file:///c.rs", "version": 1},
            "contentChanges": [{"text": "@crash\n"}],
        }),
    );
    let deadline = Instant::now() + WAIT;
    let status = loop {
        match events.recv_timeout(deadline - Instant::now()) {
            Ok(Event::Exited { status }) => break status,
            Ok(_) => {}
            Err(_) => panic!("no exit event"),
        }
    };
    assert_eq!(status, "exit code 3");
    assert!(!client.is_alive());
    assert!(
        client
            .log()
            .iter()
            .any(|l| l.contains("crashing on request"))
    );
    let (_, rx) = client.request("textDocument/hover", Value::Null);
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
}

/// Random edits on text with astral chars and CRLF: the server's copy,
/// updated only through incremental content changes, must equal the
/// editor's text after every change set.
fn incremental_changes_match_random_edits() {
    use proptest::prelude::*;
    use proptest::test_runner::{Config, TestRunner};

    let (client, _init, events) = start();
    let uri = "file:///p.rs";
    open(&client, uri, "");
    next_notification(&events, "textDocument/publishDiagnostics");
    // The runner wants an `Fn`; the version keeps rising across cases.
    let version = std::cell::Cell::new(0);

    let piece = || prop::sample::select(vec!["", "x", "😀", "\n", "\r", "\r\n", "é", "🚀z"]);
    let edit = (any::<prop::sample::Index>(), 0usize..4, piece());
    let initial = prop::collection::vec(piece(), 0..12).prop_map(|v| v.concat());
    let sets = prop::collection::vec(prop::collection::vec(edit, 1..4), 1..5);
    let mut runner = TestRunner::new(Config {
        cases: 300,
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(&(initial, sets), |(initial, sets)| {
            let send = |changes: Value| {
                version.set(version.get() + 1);
                client.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": {"uri": uri, "version": version.get()},
                        "contentChanges": changes,
                    }),
                );
            };
            // Reset the server's copy with a full-text change.
            send(json!([{ "text": initial }]));
            let errors_before = server_text(&client, uri)["errors"].as_array().map(Vec::len);
            let mut rope = Rope::from_str(&initial);
            for edits in sets {
                // One change set of sorted, non-overlapping edits.
                let len = rope.len_chars();
                let mut changes: Vec<Change> = edits
                    .iter()
                    .map(|(i, del, s)| {
                        let from = i.index(len + 1);
                        Change::replace(from, (from + del).min(len), *s)
                    })
                    .collect();
                changes.sort_by_key(|c| c.from);
                let cs = ChangeSet::from_changes_clipped(len, changes);
                send(Value::Array(content_changes(&rope, &cs)));
                cs.apply(&mut rope).unwrap();
                let got = server_text(&client, uri);
                let errors = got["errors"].as_array().map(Vec::len);
                prop_assert_eq!(errors, errors_before, "{}", got);
                let want = rope.to_string();
                prop_assert_eq!(got["text"].as_str(), Some(want.as_str()));
            }
            Ok(())
        })
        .unwrap();
}

fn missing_program_is_an_error() {
    let opts = StartOptions {
        program: "/nonexistent/maestro-no-such-server".into(),
        args: vec![],
        root: std::env::temp_dir(),
    };
    assert!(Client::start(opts, Box::new(|_| {})).is_err());
}

/// The server exits but a child it started keeps stdout open, so the
/// reader never sees end of file. The exit must still be reported.
fn exit_is_seen_while_a_child_holds_stdout() {
    let (client, _init, events) = start();
    open(&client, "file:///o.rs", "@orphan\n");
    let t = Instant::now();
    let status = loop {
        match events.recv_timeout(WAIT) {
            Ok(Event::Exited { status }) => break status,
            Ok(_) => {}
            Err(_) => panic!("no exit event"),
        }
    };
    assert_eq!(status, "exit code 4");
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "took {:?}",
        t.elapsed()
    );
    assert!(!client.is_alive());
}
