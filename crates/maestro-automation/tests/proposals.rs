//! Edit proposals end to end in process: a client creates them through the
//! server, the person at the editor reviews them with keys through
//! `Editor::handle_key`, and the client reads the outcome back through
//! `proposal.status`, `document.history` and `proposal.resolved`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use maestro_automation::{Server, Session};
use maestro_config::Config;
use maestro_core::Document;
use maestro_editor::{Editor, KeyCode, KeyEvent, Rect};
use maestro_protocol::{ErrorCode, Response};
use serde_json::{Value, json};

struct Harness {
    ed: Editor,
    server: Server,
    session: Session,
    next_id: i64,
    dir: PathBuf,
    /// Notifications collected after each request, as the transports do.
    events: Vec<maestro_protocol::Notification>,
}

impl Harness {
    /// An interactive editor (a reviewer is present) working in a fresh
    /// directory.
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("maestro-proposals-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 100, 30));
        ed.cwd = dir.clone();
        let server = Server::with_reviewer(&ed);
        let mut h = Self {
            ed,
            server,
            session: Session::default(),
            next_id: 1,
            dir,
            events: Vec::new(),
        };
        let init = h.ok("initialize", json!({"protocol_version": "1.1"}));
        assert_eq!(init["protocol_version"], "1.2");
        let caps = init["capabilities"].as_array().unwrap();
        assert!(caps.contains(&json!("proposal.review")), "{init}");
        assert!(
            init["methods"]
                .as_array()
                .unwrap()
                .contains(&json!("proposal.create"))
        );
        assert!(
            init["notifications"]
                .as_array()
                .unwrap()
                .contains(&json!("proposal.resolved"))
        );
        h.ok("events.subscribe", json!({}));
        h
    }

    /// Writes `name` with `text`, opens it in the focused view, and returns
    /// its document id.
    fn file(&mut self, name: &str, text: &str) -> u64 {
        std::fs::write(self.dir.join(name), text).unwrap();
        let r = self.ok("document.open", json!({"path": name, "focus": true}));
        r["doc"].as_u64().unwrap()
    }

    fn call(&mut self, method: &str, params: Value) -> Response {
        let id = self.next_id;
        self.next_id += 1;
        let line =
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        let out = self
            .server
            .handle_line(&mut self.ed, &mut self.session, &line)
            .expect("requests with an id get a response");
        let events = self.server.collect_events(&self.ed);
        self.events.extend(events);
        serde_json::from_str(&out).unwrap()
    }

    /// Every notification since the last call.
    fn events(&mut self) -> Vec<maestro_protocol::Notification> {
        let events = self.server.collect_events(&self.ed);
        self.events.extend(events);
        std::mem::take(&mut self.events)
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let r = self.call(method, params.clone());
        match r.result() {
            Some(v) => v.clone(),
            None => panic!("{method} {params} failed: {:?}", r.error()),
        }
    }

    fn fails(&mut self, method: &str, params: Value) -> (ErrorCode, String, Option<Value>) {
        let r = self.call(method, params.clone());
        let e = r
            .error()
            .unwrap_or_else(|| panic!("{method} {params} unexpectedly succeeded: {:?}", r.result()))
            .clone();
        (e.kind().expect("known code"), e.message, e.data)
    }

    fn text(&mut self, doc: u64) -> (String, u64) {
        let r = self.ok("document.text", json!({"doc": doc}));
        (
            r["text"].as_str().unwrap().to_string(),
            r["version"].as_u64().unwrap(),
        )
    }

    fn key(&mut self, k: KeyEvent) {
        self.ed.handle_key(k);
    }

    fn keys(&mut self, s: &str) {
        for c in s.chars() {
            self.key(KeyEvent::char(c));
        }
    }

    fn esc(&mut self) {
        self.key(KeyEvent::plain(KeyCode::Esc));
    }

    fn enter(&mut self) {
        self.key(KeyEvent::plain(KeyCode::Enter));
    }

    fn status(&self) -> String {
        self.ed
            .status
            .as_ref()
            .map(|(_, m)| m.clone())
            .unwrap_or_default()
    }

    /// `proposal.resolved` notifications since the last call.
    fn resolved(&mut self) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|n| n.method == "proposal.resolved")
            .map(|n| n.params)
            .collect()
    }

    /// Polls background work, as the terminal loop does, until proposal
    /// `id` is resolved.
    fn wait_resolved(&mut self, id: u64) -> Value {
        let start = Instant::now();
        loop {
            self.ed.poll_background();
            let st = self.ok("proposal.status", json!({"proposal": id}));
            if st["state"] == "resolved" {
                return st;
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "proposal {id} never resolved: {st}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Opens the review overlay with `space e`.
    fn open_review(&mut self) {
        self.key(KeyEvent::char(' '));
        self.key(KeyEvent::char('e'));
        assert!(self.ed.proposals.review_open(), "{}", self.status());
    }

    fn selected(&self) -> Option<u64> {
        self.ed
            .proposals
            .review
            .as_ref()
            .and_then(|v| v.selected)
            .map(|p| p.0)
    }

    fn dialog_text(&self) -> String {
        match self
            .ed
            .proposals
            .review
            .as_ref()
            .and_then(|v| v.dialog.as_ref())
        {
            Some(maestro_editor::proposals::review::Dialog::Approve { lines, .. }) => {
                lines.join("\n")
            }
            other => panic!("no approval dialog: {other:?}"),
        }
    }
}

fn create(h: &mut Harness, params: Value) -> Value {
    h.ok("proposal.create", params)
}

/// A proposal replacing chars `from..to` of `doc` with `text`.
fn edit(doc: u64, base: u64, from: u64, to: u64, text: &str) -> Value {
    json!({
        "doc": doc, "base_version": base, "encoding": "char",
        "edits": [{"range": {"start": from, "end": to}, "text": text}],
        "actor": {"name": "agent:test", "agent_id": "test-1"},
        "description": format!("write {text}"),
    })
}

fn with_verify(mut p: Value, script: &str) -> Value {
    p["verification"] = json!({"program": "sh", "args": ["-c", script]});
    p
}

fn exists(dir: &Path, name: &str) -> bool {
    dir.join(name).exists()
}

#[test]
fn create_queues_without_changing_anything_and_announces_it() {
    let mut h = Harness::new("create");
    let doc = h.file("greet.txt", "hello world\n");
    let (_, v0) = h.text(doc);
    let info = create(&mut h, edit(doc, v0, 6, 11, "there"));
    let id = info["id"].as_u64().unwrap();
    assert_eq!(info["state"], "pending");
    assert_eq!(info["decision"], Value::Null);
    assert_eq!(info["doc"], doc);
    assert_eq!(info["base_version"], v0);
    assert_eq!(info["edit_count"], 1);
    assert_eq!(
        info["actor"],
        json!({"name": "agent:test", "agent_id": "test-1"})
    );
    assert!(
        info["path"].as_str().unwrap().ends_with("greet.txt"),
        "{info}"
    );
    // Nothing changed in the buffer or on disk.
    assert_eq!(h.text(doc), ("hello world\n".into(), v0));
    assert!(!h.ed.buffers.values().any(|b| b.doc.is_modified()));
    // The status line announces it and says how to review it.
    let st = h.status();
    assert!(
        st.contains(&format!("proposal {id}")) && st.contains("space e"),
        "{st}"
    );
    assert_eq!(h.ed.proposals.pending_count(), 1);
    let list = h.ok("proposal.list", json!({}));
    assert_eq!(list["proposals"][0]["id"], id);
    let status = h.ok("proposal.status", json!({"proposal": id}));
    assert_eq!(status, info);
    // A path works as well as a doc id, and opens the file if needed
    // without moving the user's view.
    std::fs::write(h.dir.join("other.txt"), "abc\n").unwrap();
    let focused = h.ed.view().doc;
    let by_path = create(
        &mut h,
        json!({
            "path": "other.txt", "base_version": 0, "encoding": "lsp",
            "edits": [{"range": {"start": {"line": 0, "character": 0},
                                 "end": {"line": 0, "character": 1}}, "text": "A"}],
            "actor": {"name": "agent:path"}, "description": "capitalize"
        }),
    );
    assert_eq!(by_path["state"], "pending");
    assert_eq!(h.ed.view().doc, focused);
    assert_eq!(h.ed.proposals.pending_count(), 2);
}

#[test]
fn stale_or_invalid_proposals_are_refused_and_nothing_is_queued() {
    let mut h = Harness::new("refused");
    let doc = h.file("a.txt", "hello world\n");
    let (_, v0) = h.text(doc);
    h.ok(
        "document.apply_edit",
        json!({"doc": doc, "base_version": v0, "encoding": "char",
               "edits": [{"range": {"start": 0, "end": 0}, "text": ">"}], "actor": "x"}),
    );
    let (_, v1) = h.text(doc);

    // Stale: computed against the version before the edit above.
    let (code, msg, data) = h.fails("proposal.create", edit(doc, v0, 0, 1, "H"));
    assert_eq!(code, ErrorCode::StaleVersion);
    assert!(msg.contains("nothing was changed"), "{msg}");
    assert_eq!(data, Some(json!({"expected": v0, "current": v1})));

    let base = edit(doc, v1, 0, 1, "H");
    let cases: Vec<(Value, ErrorCode)> = vec![
        // Overlapping edits.
        (
            {
                let mut p = base.clone();
                p["edits"] = json!([
                    {"range": {"start": 0, "end": 4}, "text": "x"},
                    {"range": {"start": 2, "end": 6}, "text": "y"}
                ]);
                p
            },
            ErrorCode::InvalidParams,
        ),
        // Out of bounds.
        (edit(doc, v1, 0, 999, "x"), ErrorCode::InvalidParams),
        // Reversed.
        (edit(doc, v1, 5, 2, "x"), ErrorCode::InvalidParams),
        // A change that changes nothing.
        (edit(doc, v1, 3, 3, ""), ErrorCode::InvalidParams),
        // Wrong encoding for the positions.
        (
            {
                let mut p = base.clone();
                p["encoding"] = json!("lsp");
                p
            },
            ErrorCode::InvalidParams,
        ),
        // No edits, no actor name, no description.
        (
            {
                let mut p = base.clone();
                p["edits"] = json!([]);
                p
            },
            ErrorCode::InvalidParams,
        ),
        (
            {
                let mut p = base.clone();
                p["actor"] = json!({"name": "  "});
                p
            },
            ErrorCode::InvalidParams,
        ),
        (
            {
                let mut p = base.clone();
                p["description"] = json!("");
                p
            },
            ErrorCode::InvalidParams,
        ),
        // An empty verification program.
        (with_verify(base.clone(), "true"), ErrorCode::InvalidParams),
        // Both a doc and a path.
        (
            {
                let mut p = base.clone();
                p["path"] = json!("a.txt");
                p
            },
            ErrorCode::InvalidParams,
        ),
        // No such document.
        (edit(9999, 0, 0, 1, "x"), ErrorCode::NotFound),
    ];
    for (i, (mut p, want)) in cases.into_iter().enumerate() {
        if i == 8 {
            p["verification"]["program"] = json!(" ");
        }
        let (code, msg, _) = h.fails("proposal.create", p.clone());
        assert_eq!(code, want, "case {i}: {p} -> {msg}");
    }

    // A verification command needs a file on disk; a scratch buffer has none.
    let scratch = h.ed.add_document(Document::from_text("abc"));
    let (code, msg, _) = h.fails(
        "proposal.create",
        with_verify(edit(scratch.0, 0, 0, 1, "A"), "true"),
    );
    assert_eq!(code, ErrorCode::InvalidParams);
    assert!(msg.contains("no path"), "{msg}");

    assert_eq!(h.ok("proposal.list", json!({}))["proposals"], json!([]));
    assert_eq!(h.text(doc), (">hello world\n".into(), v1));
}

#[test]
fn approving_applies_one_undo_step_saves_and_runs_the_verification_command() {
    let mut h = Harness::new("approve");
    let doc = h.file("greet.txt", "hello world\n");
    let (_, v0) = h.text(doc);
    // The command checks the file on disk: it passes only if the approved
    // change was saved before it ran.
    let script = "grep -qx 'hello there' greet.txt && echo checked greet.txt";
    let info = create(&mut h, with_verify(edit(doc, v0, 6, 11, "there"), script));
    let id = info["id"].as_u64().unwrap();
    assert_eq!(info["verification"]["program"], "sh");
    assert_eq!(info["verification"]["args"], json!(["-c", script]));
    assert_eq!(info["verification"]["cwd"], h.dir.display().to_string());

    h.open_review();
    assert_eq!(h.selected(), Some(id));
    // The overlay shows the diff with intraline marks on the changed word.
    let rows = maestro_editor::proposals::review::current_rows(&h.ed);
    let changed: Vec<_> = rows
        .iter()
        .filter_map(|r| match r {
            maestro_editor::agents::review::Row::Line(l) if !l.marks.is_empty() => Some(l),
            _ => None,
        })
        .collect();
    assert_eq!(changed.len(), 2, "{rows:?}");
    assert_eq!(&changed[0].text[changed[0].marks[0].clone()], "world");
    assert_eq!(&changed[1].text[changed[1].marks[0].clone()], "there");

    // `a` asks first, and says exactly what will run, where, and that the
    // file is saved.
    h.keys("a");
    let d = h.dialog_text();
    assert!(d.contains("one undo step"), "{d}");
    assert!(d.contains("saves"), "{d}");
    assert!(
        d.contains(r"sh -c 'grep -qx '\''hello there'\'' greet.txt && echo checked greet.txt'"),
        "{d}"
    );
    assert!(d.contains(&h.dir.display().to_string()), "{d}");
    // Nothing happened yet.
    assert_eq!(h.text(doc), ("hello world\n".into(), v0));
    h.keys("y");

    // Applied, saved, and the command started without blocking.
    let (text, v1) = h.text(doc);
    assert_eq!(text, "hello there\n");
    assert!(v1 > v0);
    assert_eq!(
        std::fs::read_to_string(h.dir.join("greet.txt")).unwrap(),
        "hello there\n"
    );
    let st = h.ok("proposal.status", json!({"proposal": id}));
    assert!(
        st["state"] == "verifying" || st["state"] == "resolved",
        "{st}"
    );
    assert_eq!(st["decision"], "approved");
    assert_eq!(st["applied_version"], v1);
    assert_eq!(st["saved"], true);
    // Subscribers hear of the new version with the proposal's actor. The
    // proposal is not resolved until its command has finished, which needs
    // the editor to poll its background work first.
    let events = h.events();
    assert!(
        events.iter().any(|n| n.method == "document.changed"
            && n.params == json!({"doc": doc, "version": v1, "actor": "agent:test"})),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|n| n.method == "proposal.resolved"),
        "{events:?}"
    );

    let done = h.wait_resolved(id);
    let r = &done["verification_result"];
    assert_eq!(r["status"], "passed", "{done}");
    assert_eq!(r["exit_code"], 0);
    assert!(r["duration_ms"].as_u64().is_some());
    assert_eq!(r["output_tail"], json!(["checked greet.txt"]));
    assert_eq!(done["decided_by"]["kind"], "user");
    assert!(h.status().contains("verification passed"), "{}", h.status());

    // The notification carries the same record.
    let notes = h.resolved();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0], done);
    // And so does the history, with the edit attributed to the proposal.
    let hist = h.ok("document.history", json!({"doc": doc}));
    assert_eq!(
        hist["edits"],
        json!([{
            "actor": "agent:test", "description": "write there",
            "base_version": v0, "new_version": v1,
            "timestamp_ms": done["decided_ms"], "proposal": id
        }])
    );
    assert_eq!(hist["proposals"], json!([done]));

    // One undo reverts exactly the proposal.
    h.esc();
    assert!(!h.ed.proposals.review_open());
    h.keys("u");
    assert_eq!(h.text(doc).0, "hello world\n");
    h.keys("u");
    assert_eq!(h.text(doc).0, "hello world\n");
}

#[test]
fn a_failing_verification_is_reported_with_its_output() {
    let mut h = Harness::new("fails");
    let doc = h.file("n.txt", "one\n");
    let (_, v0) = h.text(doc);
    let script = "echo looking; grep -qx three n.txt || { echo 'not three' >&2; exit 3; }";
    let id = create(&mut h, with_verify(edit(doc, v0, 0, 3, "two"), script))["id"]
        .as_u64()
        .unwrap();
    h.open_review();
    h.keys("ay");
    let done = h.wait_resolved(id);
    let r = &done["verification_result"];
    assert_eq!(r["status"], "failed", "{done}");
    assert_eq!(r["exit_code"], 3);
    assert_eq!(r["output_tail"].as_array().unwrap().len(), 2, "{r}");
    // The change stays applied: the decision was the user's; the check is
    // information for them and the client.
    assert_eq!(done["decision"], "approved");
    assert_eq!(h.text(doc).0, "two\n");
    assert!(
        h.status().contains("verification failed (exit 3"),
        "{}",
        h.status()
    );
    // `o` shows the output in the overlay.
    h.keys("o");
    let rows = maestro_editor::proposals::review::current_rows(&h.ed);
    assert!(
        rows.iter()
            .any(|r| matches!(r, maestro_editor::agents::review::Row::Note(t) if t == "not three")),
        "{rows:?}"
    );
}

#[test]
fn a_running_verification_can_be_canceled() {
    let mut h = Harness::new("cancel");
    let doc = h.file("c.txt", "x\n");
    let id = create(&mut h, with_verify(edit(doc, 0, 0, 1, "y"), "sleep 30"))["id"]
        .as_u64()
        .unwrap();
    h.open_review();
    h.keys("ay");
    assert_eq!(
        h.ok("proposal.status", json!({"proposal": id}))["state"],
        "verifying"
    );
    let started = Instant::now();
    h.keys("c");
    let done = h.wait_resolved(id);
    assert!(started.elapsed() < Duration::from_secs(10));
    let r = &done["verification_result"];
    assert_eq!(r["status"], "failed", "{done}");
    assert_eq!(r["exit_code"], Value::Null);
    assert_eq!(r["error"], "canceled by the user");
}

#[test]
fn rejecting_sends_the_reason_and_changes_nothing() {
    let mut h = Harness::new("reject");
    let doc = h.file("r.txt", "keep me\n");
    let info = create(&mut h, with_verify(edit(doc, 0, 0, 4, "drop"), "touch ran"));
    let id = info["id"].as_u64().unwrap();
    h.open_review();
    h.keys("x");
    h.keys("wrong word");
    h.enter();
    let st = h.ok("proposal.status", json!({"proposal": id}));
    assert_eq!(st["state"], "resolved");
    assert_eq!(st["decision"], "rejected");
    assert_eq!(st["reason"], "wrong word");
    assert_eq!(st["decided_by"]["kind"], "user");
    assert_eq!(st["applied_version"], Value::Null);
    assert_eq!(st["verification_result"]["status"], "not_run");
    assert_eq!(h.text(doc), ("keep me\n".into(), 0));
    assert!(!exists(&h.dir, "ran"));
    let notes = h.resolved();
    assert_eq!(notes, vec![st.clone()]);
    let hist = h.ok("document.history", json!({"doc": doc}));
    assert_eq!(hist["edits"], json!([]));
    assert_eq!(hist["proposals"], json!([st]));
    // A decided proposal cannot be approved or rejected again.
    h.keys("a");
    assert!(
        h.status().contains("only a pending proposal"),
        "{}",
        h.status()
    );
    assert_eq!(h.text(doc).0, "keep me\n");
}

#[test]
fn withdrawing_resolves_a_pending_proposal_only() {
    let mut h = Harness::new("withdraw");
    let doc = h.file("w.txt", "abc\n");
    let id = create(&mut h, edit(doc, 0, 0, 1, "A"))["id"]
        .as_u64()
        .unwrap();
    h.open_review();
    let st = h.ok(
        "proposal.withdraw",
        json!({"proposal": id, "reason": "found a better fix"}),
    );
    assert_eq!(st["decision"], "withdrawn");
    assert_eq!(st["decided_by"], json!({"kind": "client", "name": null}));
    assert_eq!(st["reason"], "found a better fix");
    assert_eq!(h.resolved(), vec![st]);
    // Again: no longer pending.
    let (code, _, _) = h.fails("proposal.withdraw", json!({"proposal": id}));
    assert_eq!(code, ErrorCode::NotPending);
    let (code, _, _) = h.fails("proposal.withdraw", json!({"proposal": 999}));
    assert_eq!(code, ErrorCode::NotFound);
    let (code, _, _) = h.fails("proposal.status", json!({"proposal": 999}));
    assert_eq!(code, ErrorCode::NotFound);
    // The person can no longer approve it.
    h.keys("a");
    assert!(h.ed.proposals.review.as_ref().unwrap().dialog.is_none());
    assert_eq!(h.text(doc), ("abc\n".into(), 0));
}

/// Sends one request on `session`, a connection other than the harness's
/// own, and returns the response as JSON.
fn call_on(h: &mut Harness, session: &mut Session, id: i64, method: &str, params: Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let out = h
        .server
        .handle_line(&mut h.ed, session, &line)
        .expect("requests with an id get a response");
    serde_json::from_str(&out).unwrap()
}

#[test]
fn only_the_connection_that_created_a_proposal_can_withdraw_it() {
    let mut h = Harness::new("withdraw-owner");
    let doc = h.file("w.txt", "abc\n");
    let id = create(&mut h, edit(doc, 0, 0, 1, "A"))["id"]
        .as_u64()
        .unwrap();

    // A second connection, even one claiming the same actor name, cannot
    // withdraw it.
    let mut other = Session::default();
    let init = call_on(
        &mut h,
        &mut other,
        1,
        "initialize",
        json!({"protocol_version": "1.2"}),
    );
    assert!(init.get("result").is_some(), "{init}");
    let r = call_on(
        &mut h,
        &mut other,
        2,
        "proposal.withdraw",
        json!({"proposal": id, "reason": "not mine"}),
    );
    assert_eq!(r["error"]["code"], -32020, "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("another connection"),
        "{r}"
    );
    let st = h.ok("proposal.status", json!({"proposal": id}));
    assert_eq!(st["state"], "pending", "{st}");
    assert_eq!(st["decision"], Value::Null);
    assert!(h.resolved().is_empty());

    // The second connection's own proposal is its own to withdraw.
    let mut mine = edit(doc, 0, 1, 2, "B");
    mine["actor"]["name"] = json!("agent:other");
    let r = call_on(&mut h, &mut other, 3, "proposal.create", mine);
    let other_id = r["result"]["id"].as_u64().unwrap();
    let r = call_on(
        &mut h,
        &mut other,
        4,
        "proposal.withdraw",
        json!({"proposal": other_id}),
    );
    assert_eq!(r["result"]["decision"], "withdrawn", "{r}");
    // Ownership is checked before the state: the first connection gets
    // the same refusal for the other's proposal, decided or not.
    let r = h.call("proposal.withdraw", json!({"proposal": other_id}));
    assert_eq!(
        r.error().and_then(|e| e.kind()),
        Some(ErrorCode::NotOwner),
        "{r:?}"
    );

    // The creator still can.
    let st = h.ok("proposal.withdraw", json!({"proposal": id}));
    assert_eq!(st["decision"], "withdrawn");
}

#[test]
fn a_dialog_open_when_the_client_withdraws_is_closed() {
    let mut h = Harness::new("withdraw-dialog");
    let doc = h.file("w.txt", "abc\n");
    let id = create(&mut h, edit(doc, 0, 0, 1, "A"))["id"]
        .as_u64()
        .unwrap();
    h.open_review();
    h.keys("a");
    h.dialog_text();
    h.ok("proposal.withdraw", json!({"proposal": id}));
    assert!(h.ed.proposals.review.as_ref().unwrap().dialog.is_none());
    h.keys("y");
    assert_eq!(h.text(doc), ("abc\n".into(), 0));
}

#[test]
fn a_document_that_moved_on_makes_approval_stale_and_nothing_runs() {
    let mut h = Harness::new("stale");
    let doc = h.file("s.txt", "hello world\n");
    let first = create(
        &mut h,
        with_verify(edit(doc, 0, 0, 5, "HELLO"), "touch ran1"),
    )["id"]
        .as_u64()
        .unwrap();
    let second = create(
        &mut h,
        with_verify(edit(doc, 0, 6, 11, "WORLD"), "touch ran2"),
    )["id"]
        .as_u64()
        .unwrap();

    // The user types into the document after the first proposal arrived.
    h.keys("iX");
    h.esc();
    let (text, v1) = h.text(doc);
    assert_eq!(text, "Xhello world\n");

    h.open_review();
    assert_eq!(h.selected(), Some(first));
    // Stale already: approving refuses at once, no dialog to accept.
    h.keys("a");
    let st = h.ok("proposal.status", json!({"proposal": first}));
    assert_eq!(st["decision"], "stale");
    assert_eq!(st["decided_by"]["kind"], "user");
    assert!(
        st["reason"]
            .as_str()
            .unwrap()
            .contains(&format!("version {v1}")),
        "{st}"
    );
    assert_eq!(st["applied_version"], Value::Null);
    assert!(h.status().contains("stale"), "{}", h.status());
    assert_eq!(h.text(doc), ("Xhello world\n".into(), v1));
    assert!(!exists(&h.dir, "ran1"));
    let notes = h.resolved();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["decision"], "stale");
    // The other proposal waits until the person acts on it.
    assert_eq!(
        h.ok("proposal.status", json!({"proposal": second}))["state"],
        "pending"
    );
}

#[test]
fn a_change_between_the_dialog_and_the_answer_is_caught_as_stale() {
    let mut h = Harness::new("stale-dialog");
    let doc = h.file("s.txt", "hello world\n");
    let id = create(
        &mut h,
        with_verify(edit(doc, 0, 0, 5, "HELLO"), "touch ran"),
    )["id"]
        .as_u64()
        .unwrap();
    h.open_review();
    h.keys("a");
    h.dialog_text();
    // Another client edits the document while the dialog is up.
    h.ok(
        "document.apply_edit",
        json!({"doc": doc, "base_version": 0, "encoding": "char",
               "edits": [{"range": {"start": 11, "end": 11}, "text": "!"}], "actor": "other"}),
    );
    h.keys("y");
    let st = h.ok("proposal.status", json!({"proposal": id}));
    assert_eq!(st["decision"], "stale", "{st}");
    assert_eq!(h.text(doc).0, "hello world!\n");
    assert!(!exists(&h.dir, "ran"));
    assert_eq!(
        std::fs::read_to_string(h.dir.join("s.txt")).unwrap(),
        "hello world\n",
        "a stale approval must not save"
    );
}

#[test]
fn several_proposals_queue_and_are_reviewed_in_turn() {
    let mut h = Harness::new("queue");
    let a = h.file("a.txt", "alpha\n");
    let b = h.file("b.txt", "beta\n");
    let p1 = create(&mut h, edit(a, 0, 0, 5, "ALPHA"))["id"]
        .as_u64()
        .unwrap();
    let p2 = create(&mut h, edit(a, 0, 0, 1, "A"))["id"]
        .as_u64()
        .unwrap();
    let p3 = create(&mut h, edit(b, 0, 0, 4, "BETA"))["id"]
        .as_u64()
        .unwrap();
    assert_eq!(h.ed.proposals.pending_count(), 3);
    h.open_review();
    assert_eq!(h.selected(), Some(p1));
    h.keys("n");
    assert_eq!(h.selected(), Some(p2));
    h.keys("n");
    assert_eq!(h.selected(), Some(p3));
    h.keys("n");
    assert_eq!(h.selected(), Some(p1));
    h.keys("p");
    assert_eq!(h.selected(), Some(p3));

    // Approve the one for b.txt, then the first for a.txt.
    h.keys("ay");
    h.keys("n");
    assert_eq!(h.selected(), Some(p1));
    h.keys("ay");
    assert_eq!(h.text(a).0, "ALPHA\n");
    assert_eq!(h.text(b).0, "BETA\n");
    // The second for a.txt was made against the same version, which the
    // first moved on from: it is refused as stale.
    h.keys("n");
    assert_eq!(h.selected(), Some(p2));
    h.keys("a");
    assert_eq!(h.text(a).0, "ALPHA\n");
    let notes = h.resolved();
    let order: Vec<(u64, String)> = notes
        .iter()
        .map(|n| {
            (
                n["id"].as_u64().unwrap(),
                n["decision"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        order,
        vec![
            (p3, "approved".into()),
            (p1, "approved".into()),
            (p2, "stale".into())
        ]
    );
    assert_eq!(h.ed.proposals.pending_count(), 0);
    // The list filters by document.
    let list = h.ok("proposal.list", json!({"doc": a}));
    assert_eq!(list["proposals"].as_array().unwrap().len(), 2);
}

#[test]
fn the_dialog_warns_that_saving_writes_the_users_unsaved_edits() {
    let mut h = Harness::new("unsaved");
    let doc = h.file("u.txt", "one\ntwo\n");
    // The user has an unsaved edit; the agent computes against it.
    h.keys("iX");
    h.esc();
    let (text, v) = h.text(doc);
    assert_eq!(text, "Xone\ntwo\n");
    create(&mut h, with_verify(edit(doc, v, 5, 8, "TWO"), "true"));
    h.open_review();
    h.keys("a");
    let d = h.dialog_text();
    assert!(d.contains("unsaved edits"), "{d}");
    h.keys("n");
    assert!(h.ed.proposals.review.as_ref().unwrap().dialog.is_none());
    assert!(h.status().contains("canceled, nothing changed"));
    // Without a verification command nothing is saved, and the dialog says so.
    let doc2 = h.file("v.txt", "abc\n");
    let id = create(&mut h, edit(doc2, 0, 0, 1, "A"))["id"]
        .as_u64()
        .unwrap();
    h.open_review();
    while h.selected() != Some(id) {
        h.keys("n");
    }
    h.keys("a");
    let d = h.dialog_text();
    assert!(d.contains("not saved"), "{d}");
    h.keys("y");
    assert_eq!(h.text(doc2).0, "Abc\n");
    assert_eq!(
        std::fs::read_to_string(h.dir.join("v.txt")).unwrap(),
        "abc\n"
    );
    let st = h.wait_resolved(id);
    assert_eq!(st["saved"], false);
    assert_eq!(st["verification_result"], Value::Null);
}

#[test]
fn a_dialog_that_does_not_fit_cannot_be_accepted() {
    let mut h = Harness::new("small");
    let doc = h.file("f.txt", "abc\n");
    let long = "x".repeat(400);
    create(
        &mut h,
        with_verify(edit(doc, 0, 0, 1, "A"), &format!("echo {long}")),
    );
    h.open_review();
    h.keys("a");
    h.dialog_text();
    h.ed.resize(Rect::new(0, 1, 40, 10));
    h.keys("y");
    assert!(h.status().contains("too small"), "{}", h.status());
    assert_eq!(h.text(doc).0, "abc\n");
}

#[test]
fn a_replayed_macro_cannot_approve_a_proposal() {
    let mut h = Harness::new("macro");
    let doc = h.file("m.txt", "abc\n");
    let first = create(&mut h, edit(doc, 0, 0, 1, "A"))["id"]
        .as_u64()
        .unwrap();
    // The user records a macro while approving the first one by hand.
    h.keys("Q");
    h.open_review();
    h.keys("ay");
    h.esc();
    h.keys("Q");
    assert_eq!(
        h.ok("proposal.status", json!({"proposal": first}))["decision"],
        "approved"
    );
    let (_, v) = h.text(doc);
    let second = create(&mut h, edit(doc, v, 1, 2, "B"))["id"]
        .as_u64()
        .unwrap();
    // Replaying it opens the review, but its `a` and `y` are ignored.
    h.keys("q");
    assert!(h.ed.proposals.review_open());
    assert!(
        h.status().contains("replayed keys are ignored"),
        "{}",
        h.status()
    );
    assert_eq!(
        h.ok("proposal.status", json!({"proposal": second}))["state"],
        "pending"
    );
    assert_eq!(h.text(doc).0, "Abc\n");
}

#[test]
fn a_buffer_saved_under_another_name_makes_its_proposals_stale() {
    let mut h = Harness::new("renamed");
    let doc = h.file("old.txt", "abc\n");
    let id = create(&mut h, with_verify(edit(doc, 0, 0, 1, "A"), "touch ran"))["id"]
        .as_u64()
        .unwrap();
    // `:w new.txt` renames the buffer without changing its version.
    h.ok("editor.execute", json!({"line": "w new.txt"}));
    assert_eq!(h.text(doc).1, 0);
    h.open_review();
    h.keys("a");
    let st = h.ok("proposal.status", json!({"proposal": id}));
    assert_eq!(st["decision"], "stale", "{st}");
    assert!(st["reason"].as_str().unwrap().contains("saved as"), "{st}");
    assert_eq!(h.text(doc).0, "abc\n");
    assert!(!exists(&h.dir, "ran"));
}

#[test]
fn the_newest_resolution_survives_pruning() {
    // One proposal waits while 500 newer ones are made and withdrawn;
    // resolving the old one must keep its record and notification.
    let mut h = Harness::new("prune");
    let doc = h.file("p.txt", "abc\n");
    let old = create(&mut h, edit(doc, 0, 0, 1, "A"))["id"]
        .as_u64()
        .unwrap();
    for _ in 0..maestro_editor::proposals::MAX_RESOLVED {
        let id = create(&mut h, edit(doc, 0, 1, 2, "B"))["id"].clone();
        h.ok("proposal.withdraw", json!({"proposal": id}));
    }
    h.events();
    let st = h.ok("proposal.withdraw", json!({"proposal": old}));
    assert_eq!(st["decision"], "withdrawn");
    let notes = h.resolved();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["id"], old);
    let hist = h.ok("document.history", json!({"doc": doc}));
    let kept = hist["proposals"].as_array().unwrap();
    assert_eq!(kept.len(), maestro_editor::proposals::MAX_RESOLVED);
    assert!(
        kept.iter().any(|p| p["id"] == old),
        "the newest resolution was dropped"
    );
}

#[test]
fn closing_the_document_resolves_its_pending_proposals() {
    let mut h = Harness::new("close");
    let doc = h.file("c.txt", "abc\n");
    let id = create(&mut h, edit(doc, 0, 0, 1, "A"))["id"]
        .as_u64()
        .unwrap();
    h.ok("editor.execute", json!({"line": "bd"}));
    let notes = h.resolved();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["id"], id);
    assert_eq!(notes[0]["decision"], "stale");
    assert_eq!(notes[0]["decided_by"]["kind"], "editor");
    assert_eq!(notes[0]["reason"], "the document was closed");
}

#[test]
fn a_headless_server_refuses_proposals() {
    let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 80, 22));
    let doc = ed.add_document(Document::from_text("abc"));
    let mut server = Server::new(&ed);
    let mut session = Session::default();
    let mut call = |line: Value| -> Response {
        serde_json::from_str(
            &server
                .handle_line(&mut ed, &mut session, &line.to_string())
                .unwrap(),
        )
        .unwrap()
    };
    let init = call(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocol_version": "1.0"}}),
    );
    assert!(
        !init.result().unwrap()["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("proposal.review"))
    );
    let r = call(
        json!({"jsonrpc": "2.0", "id": 2, "method": "proposal.create", "params": edit(doc.0, 0, 0, 1, "A")}),
    );
    let e = r.error().unwrap();
    assert_eq!(e.kind(), Some(ErrorCode::ReviewUnavailable));
    assert!(e.message.contains("headless"), "{}", e.message);
    assert_eq!(ed.proposals.items.len(), 0);
}
