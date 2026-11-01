//! Spawns the real `maestro --headless` binary and drives the M3 acceptance
//! shape over stdin/stdout: open a file, read its version, apply a change
//! against that version, have a second change against the old version
//! refused as stale, save, check the file on disk, and read the history.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use maestro_protocol::{ErrorCode, Response, ServerMessage};
use serde_json::{Value, json};

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Client {
    fn spawn(cwd: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_maestro"))
            .args(["--headless", "--clean"])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("maestro starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Response {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
        loop {
            let mut buf = String::new();
            assert!(
                self.stdout.read_line(&mut buf).unwrap() > 0,
                "maestro closed stdout while {method} was pending"
            );
            if let ServerMessage::Response(r) = ServerMessage::parse(&buf).unwrap() {
                assert_eq!(r.id, Some(id.into()));
                return r;
            }
        }
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let r = self.call(method, params.clone());
        match r.result() {
            Some(v) => v.clone(),
            None => panic!("{method} {params} failed: {:?}", r.error()),
        }
    }
}

#[test]
fn headless_acceptance_open_edit_stale_save_history() {
    let dir = std::env::temp_dir().join(format!("maestro-cli-headless-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

    let mut c = Client::spawn(&dir);
    let init = c.ok(
        "initialize",
        json!({"protocol_version": "1.0", "client_name": "test"}),
    );
    // A 1.0 client is accepted; the server says which minor it speaks.
    assert_eq!(init["protocol_version"], "1.2");
    // Nobody can review proposals headless, so it does not offer them.
    assert!(
        !init["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("proposal.review")),
        "{init}"
    );

    // Open (relative to the editor's working directory) and read the version.
    let opened = c.ok("document.open", json!({"path": "main.rs"}));
    let doc = opened["doc"].as_u64().unwrap();
    let read = c.ok("document.text", json!({"doc": doc}));
    let v0 = read["version"].as_u64().unwrap();
    assert_eq!(read["text"], "fn main() {\n    println!(\"hello\");\n}\n");

    // Change "hello" (line 1, UTF-16 columns 14..19) against v0.
    let applied = c.ok(
        "document.apply_edit",
        json!({
            "doc": doc, "base_version": v0, "encoding": "lsp",
            "edits": [{"range": {"start": {"line": 1, "character": 14},
                                 "end": {"line": 1, "character": 19}},
                       "text": "goodbye"}],
            "actor": "agent:acceptance", "description": "change the greeting"
        }),
    );
    let v1 = applied["version"].as_u64().unwrap();
    assert!(v1 > v0);

    // A proposal needs a person to approve it; headless there is none, so
    // it is refused and nothing is queued or run.
    let refused = c.call(
        "proposal.create",
        json!({
            "doc": doc, "base_version": v1, "encoding": "char",
            "edits": [{"range": {"start": 0, "end": 2}, "text": "pub fn"}],
            "actor": {"name": "agent:proposer"}, "description": "make it public",
            "verification": {"program": "sh", "args": ["-c", "touch ran"]}
        }),
    );
    assert_eq!(
        refused.error().expect("refused headless").kind(),
        Some(ErrorCode::ReviewUnavailable)
    );
    assert!(!dir.join("ran").exists());
    let listed = c.ok("proposal.list", json!({}));
    assert_eq!(listed["proposals"], json!([]));

    // A second change computed against v0 is refused as stale.
    let stale = c.call(
        "document.apply_edit",
        json!({
            "doc": doc, "base_version": v0, "encoding": "char",
            "edits": [{"range": {"start": 0, "end": 2}, "text": "pub fn"}],
            "actor": "agent:late"
        }),
    );
    let e = stale.error().expect("stale edit must fail");
    assert_eq!(e.kind(), Some(ErrorCode::StaleVersion));
    assert_eq!(e.data, Some(json!({"expected": v0, "current": v1})));
    let read = c.ok("document.text", json!({"doc": doc}));
    assert_eq!(read["version"], v1);

    // Save and verify the bytes on disk.
    c.ok("document.save", json!({"doc": doc}));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "fn main() {\n    println!(\"goodbye\");\n}\n"
    );

    // History lists the actor whose edit landed, and only that one.
    let hist = c.ok("document.history", json!({"doc": doc}));
    let edits = hist["edits"].as_array().unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0]["actor"], "agent:acceptance");
    assert_eq!(edits[0]["description"], "change the greeting");
    assert_eq!(edits[0]["base_version"], v0);
    assert_eq!(edits[0]["new_version"], v1);

    c.ok("editor.quit", json!({}));
    let status = c.child.wait().unwrap();
    assert!(status.success(), "{status}");
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut c.child.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(stderr.contains("headless automation server"), "{stderr}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn headless_exits_cleanly_at_end_of_input() {
    let dir = std::env::temp_dir();
    let mut c = Client::spawn(&dir);
    c.ok("initialize", json!({"protocol_version": "1.0"}));
    let snap = c.ok("editor.snapshot", json!({}));
    assert_eq!(snap["mode"], "normal");
    drop(c.stdin);
    let status = c.child.wait().unwrap();
    assert!(status.success(), "{status}");
}

#[test]
fn headless_quit_leaves_the_interactive_session_alone() {
    // A scripted run in a directory must not replace or delete the session
    // an interactive editor saved there, and must not touch recent files.
    let dir = std::env::temp_dir().join(format!("maestro-headless-session-{}", std::process::id()));
    let state = dir.join("state");
    let sessions = state.join("maestro").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let saved = sessions.join("some-session.toml");
    std::fs::write(&saved, "saved by an interactive run\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "hello\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_maestro"))
        .arg("--headless")
        .current_dir(&dir)
        .env("XDG_STATE_HOME", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("maestro starts");
    let mut stdin = child.stdin.take().unwrap();
    for line in [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocol_version": "1.0"}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "document.open", "params": {"path": "notes.txt"}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "editor.execute", "params": {"line": "q"}}),
    ] {
        writeln!(stdin, "{line}").unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&saved).unwrap(),
        "saved by an interactive run\n"
    );
    let names = |d: &std::path::Path| -> Vec<String> {
        std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(
        names(&state.join("maestro")),
        vec!["sessions"],
        "no recent list written"
    );
    assert_eq!(
        names(&sessions),
        vec!["some-session.toml"],
        "no session written"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
