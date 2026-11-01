//! The live Unix-socket transport, in process: an Editor whose thread polls
//! background jobs the way the terminal loop does, and clients on other
//! threads.
//!
//! Ignored by default because sandboxes that block `AF_UNIX` sockets (the
//! agent sandbox does) fail it at `socket()`. Run it with
//! `cargo test -p maestro-automation --test socket -- --ignored`.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use maestro_automation::socket;
use maestro_config::Config;
use maestro_core::Document;
use maestro_editor::{Editor, KeyEvent, Rect};
use maestro_protocol::ServerMessage;
use serde_json::{Value, json};

/// Polls the editor (as the terminal loop does) until `done` delivers.
fn drive<T>(ed: &mut Editor, done: &mpsc::Receiver<T>) -> T {
    let start = Instant::now();
    loop {
        ed.poll_background();
        if let Ok(v) = done.try_recv() {
            return v;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn send(w: &mut UnixStream, v: Value) {
    writeln!(w, "{v}").unwrap();
}

fn read_msg(r: &mut BufReader<UnixStream>) -> ServerMessage {
    let mut line = String::new();
    assert!(r.read_line(&mut line).unwrap() > 0, "connection closed");
    ServerMessage::parse(&line).unwrap()
}

#[test]
#[ignore = "needs AF_UNIX sockets; run with --ignored outside sandboxes that block them"]
fn live_socket_serves_requests_and_events() {
    let dir = std::env::temp_dir().join(format!("maestro-sock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path: PathBuf = dir.join("nested").join("ed.sock");

    let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 80, 22));
    let doc = ed.add_document(Document::from_text("abc\n"));
    ed.switch_to(doc);
    let listener = socket::listen(&ed, &path).unwrap();

    // Owner-only socket in a directory created owner-only.
    let meta = std::fs::symlink_metadata(&path).unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let parent = std::fs::metadata(path.parent().unwrap()).unwrap();
    assert_eq!(parent.permissions().mode() & 0o777, 0o700);
    // No staging directory is left behind.
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1
    );
    // A second listener on the same path is refused.
    assert!(socket::listen(&ed, &path).is_err());

    // One-shot client, as `maestro --remote` uses.
    let (tx, rx) = mpsc::channel();
    let p = path.clone();
    std::thread::spawn(move || {
        let r = socket::remote(
            &p,
            r#"{"jsonrpc":"2.0","id":"snap","method":"editor.snapshot"}"#,
        );
        tx.send(r.map(|r| (r.line, r.is_error)).map_err(|e| e.to_string()))
            .unwrap();
    });
    let (line, is_error) = drive(&mut ed, &rx).unwrap();
    assert!(!is_error);
    let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
    assert_eq!(v["id"], "snap");
    assert_eq!(v["result"]["mode"], "normal");

    // Pipelined requests on one connection queue several jobs at once; they
    // must all be answered, in order.
    let stream = UnixStream::connect(&path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut w = stream.try_clone().unwrap();
        let mut r = BufReader::new(stream);
        let mut batch = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocol_version":"1.0"}}).to_string() + "\n";
        for i in 1..=20 {
            batch +=
                &(json!({"jsonrpc":"2.0","id":i,"method":"editor.snapshot"}).to_string() + "\n");
        }
        w.write_all(batch.as_bytes()).unwrap();
        let ids: Vec<Value> = (0..=20)
            .map(|_| match read_msg(&mut r) {
                ServerMessage::Response(resp) => serde_json::to_value(resp.id).unwrap(),
                other => panic!("{other:?}"),
            })
            .collect();
        tx.send(ids).unwrap();
    });
    let ids = drive(&mut ed, &rx);
    assert_eq!(ids, (0..=20).map(|i| json!(i)).collect::<Vec<_>>());

    // A long-lived subscribed client sees its own edit, then a keystroke.
    let stream = UnixStream::connect(&path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);
    let (tx, rx) = mpsc::channel();
    let d = doc.0;
    std::thread::spawn(move || {
        send(
            &mut w,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"1.0"}}),
        );
        let init = read_msg(&mut r);
        send(
            &mut w,
            json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe"}),
        );
        let sub = read_msg(&mut r);
        send(
            &mut w,
            json!({"jsonrpc":"2.0","id":3,"method":"document.apply_edit","params":{
            "doc": d, "base_version": 0, "encoding": "char", "actor": "agent:sock",
            "edits": [{"range": {"start": 0, "end": 0}, "text": ">"}]}}),
        );
        let applied = read_msg(&mut r);
        let changed = read_msg(&mut r);
        tx.send((init, sub, applied, changed, w, r)).unwrap();
    });
    let (init, sub, applied, changed, mut w, mut r) = drive(&mut ed, &rx);
    for m in [&init, &sub, &applied] {
        match m {
            ServerMessage::Response(resp) => assert!(resp.error().is_none(), "{resp:?}"),
            other => panic!("expected a response, got {other:?}"),
        }
    }
    let ServerMessage::Notification(n) = changed else {
        panic!("expected a notification");
    };
    assert_eq!(n.method, "document.changed");
    assert_eq!(
        n.params,
        json!({"doc": d, "version": 1, "actor": "agent:sock"})
    );
    assert_eq!(ed.doc().text().to_string(), ">abc\n");

    // The user deletes a character; the event poll reports it with no actor.
    ed.handle_key(KeyEvent::char('d'));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let m = read_msg(&mut r);
        tx.send((m, r)).unwrap();
    });
    let (m, r) = drive(&mut ed, &rx);
    let ServerMessage::Notification(n) = m else {
        panic!("expected a notification");
    };
    assert_eq!(n.params, json!({"doc": d, "version": 2, "actor": null}));

    // editor.quit's response arrives before the editor stops.
    let (tx, rx) = mpsc::channel();
    let mut r = r;
    std::thread::spawn(move || {
        send(
            &mut w,
            json!({"jsonrpc":"2.0","id":4,"method":"editor.quit","params":{"force":true}}),
        );
        loop {
            if let ServerMessage::Response(resp) = read_msg(&mut r) {
                tx.send(resp).unwrap();
                return;
            }
        }
    });
    let resp = drive(&mut ed, &rx);
    assert!(resp.error().is_none(), "{resp:?}");
    assert!(ed.should_quit);

    drop(listener);
    assert!(!path.exists(), "socket file removed on drop");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[ignore = "needs AF_UNIX sockets; run with --ignored outside sandboxes that block them"]
fn listen_never_replaces_files_and_bounds_request_lines() {
    let dir = std::env::temp_dir().join(format!("maestro-sock2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 80, 22));

    // A regular file at the path is refused and left alone.
    let file = dir.join("not-a-socket");
    std::fs::write(&file, "keep me").unwrap();
    assert!(socket::listen(&ed, &file).is_err());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep me");

    // A stale socket (bound, then its listener closed) is replaced.
    let path = dir.join("ed.sock");
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    let listener = socket::listen(&ed, &path).unwrap();

    // A request line over the limit gets an error and the connection closes.
    let stream = UnixStream::connect(&path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut w = stream.try_clone().unwrap();
        let mut r = BufReader::new(stream);
        let big = vec![b' '; socket::MAX_LINE + 10];
        // The server may close before reading all of it.
        let _ = w.write_all(&big);
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let mut rest = String::new();
        let eof = r.read_line(&mut rest).unwrap() == 0;
        tx.send((line, eof)).unwrap();
    });
    let (line, eof) = drive(&mut ed, &rx);
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["error"]["code"], -32600, "{line}");
    assert!(eof, "connection closed after the oversized line");

    drop(listener);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[ignore = "needs AF_UNIX sockets; run with --ignored outside sandboxes that block them"]
fn proposal_over_the_socket_is_reviewed_applied_verified_and_recorded() {
    let dir = std::env::temp_dir().join(format!("maestro-sock3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ed.sock");
    let file = dir.join("greet.txt");
    std::fs::write(&file, "hello world\n").unwrap();

    let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 100, 30));
    ed.cwd = dir.clone();
    ed.open(&file).unwrap();
    let d = ed.view().doc.0;
    let listener = socket::listen(&ed, &path).unwrap();

    // A client proposes a change with a verification command that checks
    // the file on disk.
    let stream = UnixStream::connect(&path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        send(
            &mut w,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"1.1"}}),
        );
        let init = read_msg(&mut r);
        send(
            &mut w,
            json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe"}),
        );
        let _ = read_msg(&mut r);
        send(
            &mut w,
            json!({"jsonrpc":"2.0","id":3,"method":"proposal.create","params":{
                "doc": d, "base_version": 0, "encoding": "char",
                "edits": [{"range": {"start": 6, "end": 11}, "text": "there"}],
                "actor": {"name": "agent:sock", "agent_id": "sock-1"},
                "description": "greet someone closer",
                "verification": {"program": "sh", "args": ["-c", "grep -qx 'hello there' greet.txt"]}
            }}),
        );
        let created = loop {
            if let ServerMessage::Response(resp) = read_msg(&mut r) {
                break resp;
            }
        };
        tx.send((init, created, w, r)).unwrap();
    });
    let (init, created, mut w, mut r) = drive(&mut ed, &rx);
    let ServerMessage::Response(init) = init else {
        panic!("{init:?}")
    };
    assert!(
        init.result().unwrap()["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("proposal.review"))
    );
    let created = created.result().cloned().expect("created");
    assert_eq!(created["state"], "pending");
    let id = created["id"].as_u64().unwrap();
    assert_eq!(ed.doc().text().to_string(), "hello world\n");

    // The person reviews and approves it with keys.
    for k in [' ', 'e', 'a', 'y'] {
        ed.handle_key(KeyEvent::char(k));
    }
    assert_eq!(ed.doc().text().to_string(), "hello there\n");

    // The client hears that it resolved, then reads the history.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let resolved = loop {
            if let ServerMessage::Notification(n) = read_msg(&mut r)
                && n.method == "proposal.resolved"
            {
                break n.params;
            }
        };
        send(
            &mut w,
            json!({"jsonrpc":"2.0","id":4,"method":"document.history","params":{"doc": d}}),
        );
        let hist = loop {
            if let ServerMessage::Response(resp) = read_msg(&mut r) {
                break resp;
            }
        };
        tx.send((resolved, hist)).unwrap();
    });
    let (resolved, hist) = drive(&mut ed, &rx);
    assert_eq!(resolved["id"], id);
    assert_eq!(resolved["decision"], "approved");
    assert_eq!(
        resolved["verification_result"]["status"], "passed",
        "{resolved}"
    );
    assert_eq!(resolved["verification_result"]["exit_code"], 0);
    let hist = hist.result().cloned().unwrap();
    assert_eq!(hist["edits"][0]["proposal"], id);
    assert_eq!(hist["edits"][0]["actor"], "agent:sock");
    assert_eq!(hist["proposals"][0], resolved);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello there\n");

    drop(listener);
    std::fs::remove_dir_all(&dir).unwrap();
}
