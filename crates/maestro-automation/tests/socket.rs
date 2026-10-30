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
