//! Drives the automation server against an in-process Editor.

use std::path::PathBuf;

use maestro_automation::{Server, Session};
use maestro_config::Config;
use maestro_core::Document;
use maestro_editor::{DocumentId, Editor, KeyCode, KeyEvent, Rect};
use maestro_protocol::{ErrorCode, Response};
use serde_json::{Value, json};

struct Harness {
    ed: Editor,
    server: Server,
    session: Session,
    next_id: i64,
}

impl Harness {
    fn new() -> Self {
        let ed = Editor::new(Config::default(), Rect::new(0, 1, 80, 22));
        let server = Server::new(&ed);
        let mut h = Self {
            ed,
            server,
            session: Session::default(),
            next_id: 1,
        };
        h.ok("initialize", json!({"protocol_version": "1.0"}));
        h
    }

    /// A harness whose focused view shows a scratch document with `text`.
    fn with_text(text: &str) -> (Self, u64) {
        let mut h = Self::new();
        let id = h.ed.add_document(Document::from_text(text));
        h.ed.switch_to(id);
        (h, id.0)
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
        serde_json::from_str(&out).unwrap()
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
}

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("maestro-automation-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn methods_before_initialize_are_refused_and_major_versions_checked() {
    let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 80, 22));
    let mut server = Server::new(&ed);
    let mut session = Session::default();
    let mut call = |line: &str| -> Response {
        serde_json::from_str(&server.handle_line(&mut ed, &mut session, line).unwrap()).unwrap()
    };
    let r = call(r#"{"jsonrpc":"2.0","id":1,"method":"editor.snapshot"}"#);
    assert_eq!(r.error().unwrap().kind(), Some(ErrorCode::NotInitialized));
    let r = call(
        r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocol_version":"2.1"}}"#,
    );
    let e = r.error().unwrap();
    assert_eq!(e.kind(), Some(ErrorCode::UnsupportedProtocolVersion));
    assert_eq!(e.data.as_ref().unwrap()["supported"], "1.0");
    let r = call(
        r#"{"jsonrpc":"2.0","id":3,"method":"initialize","params":{"protocol_version":"1.7"}}"#,
    );
    let v = r.result().unwrap();
    assert_eq!(v["protocol_version"], "1.0");
    assert!(
        v["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("document.apply_edit"))
    );
    let r = call(r#"{"jsonrpc":"2.0","id":4,"method":"no.such.method"}"#);
    assert_eq!(r.error().unwrap().kind(), Some(ErrorCode::MethodNotFound));
    let r = call(r#"{"jsonrpc":"1.0","id":5,"method":"editor.snapshot"}"#);
    assert_eq!(r.error().unwrap().kind(), Some(ErrorCode::InvalidRequest));
    let r = call("{not json");
    assert_eq!(r.error().unwrap().kind(), Some(ErrorCode::ParseError));
    assert_eq!(r.id, None);
    // A request without an id is a notification: it runs, nothing comes back.
    assert!(
        server
            .handle_line(
                &mut ed,
                &mut session,
                r#"{"jsonrpc":"2.0","method":"editor.snapshot"}"#
            )
            .is_none()
    );
}

#[test]
fn snapshot_shape() {
    let (mut h, doc) = Harness::with_text("one\ntwo\n");
    h.ed.handle_key(KeyEvent::char('w'));
    let s = h.ok("editor.snapshot", json!({}));
    let bufs = s["buffers"].as_array().unwrap();
    let b = bufs.iter().find(|b| b["id"] == doc).unwrap();
    assert_eq!(b["path"], Value::Null);
    assert_eq!(b["name"], "[scratch]");
    assert_eq!(b["version"], 0);
    assert_eq!(b["modified"], false);
    assert_eq!(b["line_count"], 2);
    assert_eq!(b["char_count"], 8);
    assert_eq!(b["read_only"], Value::Null);

    let focused = s["focused_view"].as_u64().unwrap();
    let views = s["views"].as_array().unwrap();
    assert_eq!(views.len(), 1);
    let v = &views[0];
    assert_eq!(v["id"], focused);
    assert_eq!(v["doc"], doc);
    assert_eq!(v["top_line"], 0);
    assert_eq!(v["primary"], 0);
    // `w` selects "one" plus the newline in the Maestro profile.
    let sel = &v["selections"][0];
    assert_eq!(sel["anchor"], 0);
    assert!(sel["head"].as_u64().unwrap() > 0);
    assert_eq!(sel["anchor_lsp"], json!({"line": 0, "character": 0}));
    assert_eq!(s["mode"], "normal");
}

#[test]
fn apply_edit_success_and_stale_refusal_change_nothing() {
    let (mut h, doc) = Harness::with_text("hello world\n");
    let (_, v0) = h.text(doc);
    let r = h.ok(
        "document.apply_edit",
        json!({
            "doc": doc, "base_version": v0, "encoding": "char",
            "edits": [
                {"range": {"start": 6, "end": 11}, "text": "there"},
                {"range": {"start": 0, "end": 0}, "text": ">> "}
            ],
            "actor": "agent:test", "description": "greet"
        }),
    );
    let v1 = r["version"].as_u64().unwrap();
    assert_eq!(r["base_version"], v0);
    assert!(v1 > v0);
    assert_eq!(h.text(doc), (">> hello there\n".to_string(), v1));

    // A second edit computed against v0 is refused and changes nothing.
    let (code, msg, data) = h.fails(
        "document.apply_edit",
        json!({
            "doc": doc, "base_version": v0, "encoding": "char",
            "edits": [{"range": {"start": 0, "end": 5}, "text": "bye"}],
            "actor": "agent:late"
        }),
    );
    assert_eq!(code, ErrorCode::StaleVersion);
    assert!(msg.contains("nothing was changed"), "{msg}");
    assert_eq!(data.unwrap(), json!({"expected": v0, "current": v1}));
    assert_eq!(h.text(doc), (">> hello there\n".to_string(), v1));
    let hist = h.ok("document.history", json!({"doc": doc}));
    assert_eq!(hist["edits"].as_array().unwrap().len(), 1);
}

#[test]
fn overlapping_out_of_bounds_and_mismatched_edits_are_invalid_params() {
    let (mut h, doc) = Harness::with_text("abcdef");
    let base = json!({"doc": doc, "base_version": 0, "encoding": "char", "actor": "a"});
    let with = |edits: Value| {
        let mut p = base.clone();
        p["edits"] = edits;
        p
    };
    let cases = [
        // Overlap.
        json!([{"range": {"start": 0, "end": 3}, "text": "x"},
               {"range": {"start": 2, "end": 4}, "text": "y"}]),
        // Out of bounds.
        json!([{"range": {"start": 4, "end": 7}, "text": "x"}]),
        // Reversed.
        json!([{"range": {"start": 3, "end": 1}, "text": "x"}]),
        // LSP position under a char encoding.
        json!([{"range": {"start": {"line": 0, "character": 0}, "end": 1}, "text": "x"}]),
        // Empty batch.
        json!([]),
    ];
    for edits in cases {
        let (code, msg, _) = h.fails("document.apply_edit", with(edits.clone()));
        assert_eq!(code, ErrorCode::InvalidParams, "{edits}: {msg}");
        assert_eq!(h.text(doc), ("abcdef".to_string(), 0), "{edits}");
    }
    // Touching ranges are fine, in any order.
    h.ok(
        "document.apply_edit",
        with(json!([{"range": {"start": 3, "end": 6}, "text": "Y"},
                    {"range": {"start": 0, "end": 3}, "text": "X"}])),
    );
    assert_eq!(h.text(doc).0, "XY");
    // Empty actor is refused.
    let (code, _, _) = h.fails(
        "document.apply_edit",
        json!({"doc": doc, "base_version": 1, "encoding": "char", "actor": " ",
               "edits": [{"range": {"start": 0, "end": 0}, "text": "z"}]}),
    );
    assert_eq!(code, ErrorCode::InvalidParams);
    let (code, _, _) = h.fails(
        "document.apply_edit",
        json!({"doc": 999, "base_version": 0, "encoding": "char", "actor": "a",
               "edits": [{"range": {"start": 0, "end": 0}, "text": "z"}]}),
    );
    assert_eq!(code, ErrorCode::NotFound);
}

#[test]
fn lsp_positions_count_utf16_units_around_astral_chars() {
    // U+1F600 is one char but two UTF-16 code units.
    let (mut h, doc) = Harness::with_text("a😀b\nx😀y\n");
    h.ok(
        "document.apply_edit",
        json!({
            "doc": doc, "base_version": 0, "encoding": "lsp",
            "edits": [
                {"range": {"start": {"line": 1, "character": 1},
                           "end": {"line": 1, "character": 3}}, "text": "Z"},
                {"range": {"start": {"line": 0, "character": 3},
                           "end": {"line": 0, "character": 4}}, "text": "B"}
            ],
            "actor": "lsp-client"
        }),
    );
    assert_eq!(h.text(doc).0, "a😀B\nxZy\n");

    // Reading a range in LSP coordinates.
    let r = h.ok(
        "document.text",
        json!({"doc": doc, "encoding": "lsp",
               "range": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 3}}}),
    );
    assert_eq!(r["text"], "😀");

    // Selections report both encodings: char 2 is UTF-16 unit 3.
    let view = h.ed.focus.0;
    let r = h.ok(
        "selections.set",
        json!({"view": view, "encoding": "char", "ranges": [{"anchor": 2, "head": 2}]}),
    );
    assert_eq!(
        r["selections"][0]["head_lsp"],
        json!({"line": 0, "character": 3})
    );

    // A line past the end is out of bounds.
    let (code, _, _) = h.fails(
        "document.apply_edit",
        json!({"doc": doc, "base_version": 1, "encoding": "lsp", "actor": "a",
               "edits": [{"range": {"start": {"line": 9, "character": 0},
                                    "end": {"line": 9, "character": 0}}, "text": "z"}]}),
    );
    assert_eq!(code, ErrorCode::InvalidParams);
}

#[test]
fn one_undo_step_reverts_an_applied_edit() {
    let (mut h, doc) = Harness::with_text("alpha beta gamma\n");
    h.ok(
        "document.apply_edit",
        json!({"doc": doc, "base_version": 0, "encoding": "char", "actor": "agent",
               "edits": [{"range": {"start": 0, "end": 5}, "text": "ALPHA"},
                         {"range": {"start": 11, "end": 16}, "text": "GAMMA"}]}),
    );
    assert_eq!(h.text(doc).0, "ALPHA beta GAMMA\n");
    // The user presses `u` once.
    h.ed.handle_key(KeyEvent::char('u'));
    assert_eq!(h.text(doc).0, "alpha beta gamma\n");
    // The same works through the API.
    h.ed.handle_key(KeyEvent::char('U'));
    assert_eq!(h.text(doc).0, "ALPHA beta GAMMA\n");
    h.ok("command.run", json!({"name": "undo"}));
    assert_eq!(h.text(doc).0, "alpha beta gamma\n");
}

#[test]
fn an_agent_edit_during_typing_is_its_own_undo_step() {
    let (mut h, doc) = Harness::with_text("0123456789\n");
    // The user starts typing at the start of the line.
    for k in ['i', 'a', 'b'] {
        h.ed.handle_key(KeyEvent::char(k));
    }
    let (_, v) = h.text(doc);
    // An agent appends at the end of the line meanwhile.
    h.ok(
        "document.apply_edit",
        json!({"doc": doc, "base_version": v, "encoding": "char", "actor": "agent",
               "edits": [{"range": {"start": 12, "end": 12}, "text": "!"}]}),
    );
    // The user's cursor moved with the text and typing continues.
    h.ed.handle_key(KeyEvent::char('c'));
    h.ed.handle_key(KeyEvent::plain(KeyCode::Esc));
    assert_eq!(h.text(doc).0, "abc0123456789!\n");
    h.ed.handle_key(KeyEvent::char('u'));
    assert_eq!(h.text(doc).0, "ab0123456789!\n");
    h.ed.handle_key(KeyEvent::char('u'));
    assert_eq!(h.text(doc).0, "ab0123456789\n");
    h.ed.handle_key(KeyEvent::char('u'));
    assert_eq!(h.text(doc).0, "0123456789\n");
}

#[test]
fn history_records_what_the_request_gave() {
    let (mut h, doc) = Harness::with_text("x");
    h.ok(
        "document.apply_edit",
        json!({"doc": doc, "base_version": 0, "encoding": "char", "actor": "agent:one",
               "description": "first", "edits": [{"range": {"start": 1, "end": 1}, "text": "y"}]}),
    );
    // Typing does not appear in the API history.
    h.ed.handle_key(KeyEvent::char('A'));
    h.ed.handle_key(KeyEvent::char('z'));
    h.ed.handle_key(KeyEvent::plain(KeyCode::Esc));
    let (_, v) = h.text(doc);
    h.ok(
        "document.apply_edit",
        json!({"doc": doc, "base_version": v, "encoding": "char", "actor": "agent:two",
               "edits": [{"range": {"start": 0, "end": 1}, "text": "X"}]}),
    );
    let hist = h.ok("document.history", json!({"doc": doc}));
    let edits = hist["edits"].as_array().unwrap();
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[0]["actor"], "agent:one");
    assert_eq!(edits[0]["description"], "first");
    assert_eq!(edits[0]["base_version"], 0);
    assert_eq!(edits[0]["new_version"], 1);
    assert!(edits[0]["timestamp_ms"].as_u64().unwrap() > 1_600_000_000_000);
    assert_eq!(edits[1]["actor"], "agent:two");
    assert_eq!(edits[1]["description"], Value::Null);
    assert_eq!(edits[1]["base_version"], v);
    assert_eq!(edits[1]["new_version"], v + 1);
    assert_eq!(h.text(doc).0, "Xyz");
}

#[test]
fn command_run_moves_selections() {
    let (mut h, doc) = Harness::with_text("hello world\nsecond line\n");
    let view = h.ed.focus.0;
    let r = h.ok(
        "command.run",
        json!({"name": "move_char_right", "count": 3}),
    );
    assert_eq!(r["mode"], "normal");
    let s = h.ok("selections.get", json!({"view": view}));
    assert_eq!(s["doc"], doc);
    assert_eq!(s["selections"][0]["head"], 3);
    h.ok("command.run", json!({"name": "move_line_down"}));
    let s = h.ok("selections.get", json!({"view": view}));
    assert_eq!(s["selections"][0]["head"], 15);
    assert_eq!(
        s["selections"][0]["head_lsp"],
        json!({"line": 1, "character": 3})
    );
    // A command that edits goes through the same path as its key.
    h.ok("command.run", json!({"name": "goto_file_start"}));
    h.ok("command.run", json!({"name": "delete_selection"}));
    assert_eq!(h.text(doc).0, "ello world\nsecond line\n");

    let (code, _, _) = h.fails("command.run", json!({"name": "no_such_command"}));
    assert_eq!(code, ErrorCode::NotFound);
    // A command that waits for a typed character is refused and canceled.
    let (code, msg, _) = h.fails("command.run", json!({"name": "find_next_char"}));
    assert_eq!(code, ErrorCode::CommandFailed);
    assert!(msg.contains("typed character"), "{msg}");
    assert!(h.ed.pending_char_label().is_none());
    // So is one that opens a prompt.
    let (code, _, _) = h.fails("command.run", json!({"name": "search"}));
    assert_eq!(code, ErrorCode::CommandFailed);
    assert!(h.ed.prompt.is_none());
}

#[test]
fn command_list_includes_docs_and_keys() {
    let mut h = Harness::new();
    let r = h.ok("command.list", json!({}));
    let cmds = r["commands"].as_array().unwrap();
    let undo = cmds.iter().find(|c| c["name"] == "undo").unwrap();
    assert_eq!(undo["kind"], "command");
    assert_eq!(undo["doc"], "Undo");
    assert!(
        undo["keys"]
            .as_array()
            .unwrap()
            .contains(&json!({"mode": "normal", "keys": "u"}))
    );
    let write = cmds.iter().find(|c| c["name"] == "write").unwrap();
    assert_eq!(write["kind"], "typed");
    assert!(write["aliases"].as_array().unwrap().contains(&json!("w")));
    assert_eq!(
        cmds.len(),
        maestro_editor::commands::all().count() + maestro_editor::typed::all().len()
    );
}

#[test]
fn execute_runs_colon_commands_and_reports_errors() {
    let (mut h, doc) = Harness::with_text("cat cat dog\n");
    h.ok("editor.execute", json!({"line": "%s/cat/cow/g"}));
    assert_eq!(h.text(doc).0, "cow cow dog\n");
    let (code, msg, _) = h.fails("editor.execute", json!({"line": "frobnicate"}));
    assert_eq!(code, ErrorCode::CommandFailed);
    assert!(msg.contains("unknown command"), "{msg}");
}

#[test]
fn open_save_and_quit() {
    let dir = temp_dir("save");
    let path = dir.join("f.txt");
    std::fs::write(&path, "before\n").unwrap();
    let mut h = Harness::new();
    let focused_doc = h.ed.view().doc;
    let r = h.ok("document.open", json!({"path": path.display().to_string()}));
    let doc = r["doc"].as_u64().unwrap();
    assert_eq!(r["already_open"], false);
    // Opening does not move the user's view unless asked.
    assert_eq!(h.ed.view().doc, focused_doc);
    let again = h.ok(
        "document.open",
        json!({"path": path.display().to_string(), "focus": true}),
    );
    assert_eq!(again["doc"], doc);
    assert_eq!(again["already_open"], true);
    assert_eq!(h.ed.view().doc, DocumentId(doc));

    h.ok(
        "document.apply_edit",
        json!({"doc": doc, "base_version": 0, "encoding": "char", "actor": "a",
               "edits": [{"range": {"start": 0, "end": 6}, "text": "after"}]}),
    );
    // Quitting with unsaved changes is refused.
    let (code, _, data) = h.fails("editor.quit", json!({}));
    assert_eq!(code, ErrorCode::UnsavedChanges);
    assert!(data.unwrap().to_string().contains("f.txt"));
    assert!(!h.ed.should_quit);

    let r = h.ok("document.save", json!({"doc": doc}));
    assert_eq!(r["path"], path.display().to_string());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "after\n");
    let snap = h.ok("editor.snapshot", json!({}));
    let b = snap["buffers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == doc)
        .unwrap()
        .clone();
    assert_eq!(b["modified"], false);

    // Saving over a file that changed on disk is a conflict unless forced.
    h.ok(
        "document.apply_edit",
        json!({"doc": doc, "base_version": 1, "encoding": "char", "actor": "a",
               "edits": [{"range": {"start": 0, "end": 0}, "text": "+"}]}),
    );
    std::fs::write(&path, "someone else, longer\n").unwrap();
    let (code, _, _) = h.fails("document.save", json!({"doc": doc}));
    assert_eq!(code, ErrorCode::Conflict);
    h.ok("document.save", json!({"doc": doc, "force": true}));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "+after\n");

    let (code, _, _) = h.fails("document.open", json!({"path": dir.display().to_string()}));
    assert_eq!(code, ErrorCode::InvalidParams);
    h.ok("editor.quit", json!({}));
    assert!(h.ed.should_quit);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn selections_set_normalizes_and_validates() {
    let (mut h, _doc) = Harness::with_text("abc\ndef\n");
    let view = h.ed.focus.0;
    let r = h.ok(
        "selections.set",
        json!({"view": view, "encoding": "lsp", "primary": 0, "ranges": [
            {"anchor": {"line": 1, "character": 0}, "head": {"line": 1, "character": 2}},
            {"anchor": {"line": 0, "character": 1}, "head": {"line": 0, "character": 1}}
        ]}),
    );
    // Sorted by position; the primary follows its range.
    assert_eq!(r["selections"][0]["anchor"], 1);
    assert_eq!(r["selections"][1]["anchor"], 4);
    assert_eq!(r["selections"][1]["head"], 6);
    assert_eq!(r["primary"], 1);
    let (code, _, _) = h.fails(
        "selections.set",
        json!({"view": view, "encoding": "char", "ranges": [{"anchor": 0, "head": 99}]}),
    );
    assert_eq!(code, ErrorCode::InvalidParams);
    let (code, _, _) = h.fails("selections.get", json!({"view": 12345}));
    assert_eq!(code, ErrorCode::NotFound);
}

#[test]
fn headless_transport_streams_responses_and_events() {
    let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 80, 22));
    let doc = ed.add_document(Document::from_text("abc\n"));
    ed.switch_to(doc);
    let d = doc.0;
    let input = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocol_version": "1.0"}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "events.subscribe"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "document.apply_edit", "params": {
            "doc": d, "base_version": 0, "encoding": "char", "actor": "agent:x",
            "edits": [{"range": {"start": 0, "end": 0}, "text": ">"}]}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "command.run", "params": {"name": "delete_selection"}}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "editor.quit", "params": {"force": true}}),
        json!({"jsonrpc": "2.0", "id": 6, "method": "editor.snapshot"}),
    ]
    .iter()
    .map(|v| v.to_string() + "\n")
    .collect::<String>();
    let mut out = Vec::new();
    maestro_automation::headless::serve(&mut ed, input.as_bytes(), &mut out).unwrap();
    let lines: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let methods: Vec<String> = lines
        .iter()
        .map(|l| match l.get("method") {
            Some(m) => m.as_str().unwrap().to_string(),
            None => format!("response {}", l["id"]),
        })
        .collect();
    assert_eq!(
        methods,
        [
            "response 1",
            "response 2",
            "response 3",
            "document.changed",
            "response 4",
            "document.changed",
            "response 5",
        ],
        "{lines:#?}"
    );
    assert_eq!(
        lines[3]["params"],
        json!({"doc": d, "version": 1, "actor": "agent:x"})
    );
    // The command's change did not come through apply_edit: no actor.
    assert_eq!(
        lines[5]["params"],
        json!({"doc": d, "version": 2, "actor": null})
    );
    // The loop stopped at quit; request 6 was never read.
    assert!(ed.should_quit);
}

#[test]
fn an_edit_while_a_regex_prompt_is_open_does_not_break_the_prompt() {
    let (mut h, doc) = Harness::with_text("0123456789\n");
    // The cursor is on the 8, and the user opens the select-regex prompt,
    // which remembers that selection.
    for _ in 0..8 {
        h.ed.handle_key(KeyEvent::char('l'));
    }
    h.ed.handle_key(KeyEvent::char('s'));
    assert!(h.ed.prompt.is_some());
    // An agent deletes everything meanwhile.
    h.ok(
        "document.apply_edit",
        json!({"doc": doc, "base_version": 0, "encoding": "char", "actor": "agent",
               "edits": [{"range": {"start": 0, "end": 11}, "text": ""}]}),
    );
    // Typing into the prompt previews against the moved selection instead
    // of slicing past the end of the text.
    h.ed.handle_key(KeyEvent::char('1'));
    h.ed.handle_key(KeyEvent::plain(KeyCode::Esc));
    assert!(h.ed.prompt.is_none());
    assert_eq!(h.text(doc).0, "");
}

#[test]
fn edits_that_change_nothing_are_refused_and_not_recorded() {
    let (mut h, doc) = Harness::with_text("abc");
    let (code, msg, _) = h.fails(
        "document.apply_edit",
        json!({"doc": doc, "base_version": 0, "encoding": "char", "actor": "a",
               "edits": [{"range": {"start": 1, "end": 1}, "text": ""}]}),
    );
    assert_eq!(code, ErrorCode::InvalidParams);
    assert!(msg.contains("change nothing"), "{msg}");
    assert_eq!(h.text(doc), ("abc".to_string(), 0));
    let hist = h.ok("document.history", json!({"doc": doc}));
    assert!(hist["edits"].as_array().unwrap().is_empty());
}

#[test]
fn commands_are_refused_while_the_user_is_mid_input() {
    let (mut h, doc) = Harness::with_text("abc abc\n");
    // The user has typed "/ab" into the search prompt.
    for k in ['/', 'a', 'b'] {
        h.ed.handle_key(KeyEvent::char(k));
    }
    let (code, msg, _) = h.fails("command.run", json!({"name": "find_next_char"}));
    assert_eq!(code, ErrorCode::CommandFailed);
    assert!(msg.contains("waiting for the user's input"), "{msg}");
    let (code, _, _) = h.fails("editor.execute", json!({"line": "%s/abc/x/g"}));
    assert_eq!(code, ErrorCode::CommandFailed);
    // The user's prompt is untouched and nothing is armed behind it.
    assert_eq!(h.ed.prompt.as_ref().unwrap().input, "ab");
    assert!(h.ed.pending_char_label().is_none());
    assert_eq!(h.text(doc).0, "abc abc\n");
    // Version-checked edits still apply.
    h.ok(
        "document.apply_edit",
        json!({"doc": doc, "base_version": 0, "encoding": "char", "actor": "a",
               "edits": [{"range": {"start": 7, "end": 7}, "text": "!"}]}),
    );
    assert_eq!(h.text(doc).0, "abc abc!\n");
}

#[test]
fn lsp_columns_past_the_line_end_clamp_even_when_huge() {
    let (mut h, doc) = Harness::with_text("a\nb");
    h.ok(
        "document.apply_edit",
        json!({"doc": doc, "base_version": 0, "encoding": "lsp", "actor": "a",
               "edits": [{"range": {"start": {"line": 0, "character": 4294967295u32},
                                    "end": {"line": 0, "character": 4294967295u32}},
                          "text": "!"}]}),
    );
    assert_eq!(h.text(doc).0, "a!\nb");
}

#[cfg(unix)]
#[test]
fn opening_something_other_than_a_regular_file_is_refused() {
    let mut h = Harness::new();
    let (code, msg, _) = h.fails("document.open", json!({"path": "/dev/null"}));
    assert_eq!(code, ErrorCode::InvalidParams);
    assert!(msg.contains("not a regular file"), "{msg}");
}

#[test]
fn agent_edits_keep_the_syntax_tree_in_sync() {
    let dir = temp_dir("syntax-sync");
    let path = dir.join("lib.rs");
    std::fs::write(&path, "fn a() {}\n").unwrap();
    let mut h = Harness::new();
    let opened = h.ok("document.open", json!({"path": path.to_str().unwrap()}));
    let doc = opened["doc"].as_u64().unwrap();
    h.ok(
        "document.apply_edit",
        json!({"doc": doc, "base_version": 0, "encoding": "char",
               "edits": [{"range": {"start": 3, "end": 4}, "text": "renamed"}],
               "actor": "test"}),
    );
    let buf = &h.ed.buffers[&DocumentId(doc)];
    let text = buf.doc.text();
    assert_eq!(text.to_string(), "fn renamed() {}\n");
    let fresh = maestro_syntax::Syntax::parse(buf.syntax.language().unwrap(), text).unwrap();
    assert_eq!(
        buf.syntax.syntax().unwrap().tree().root_node().to_sexp(),
        fresh.tree().root_node().to_sexp()
    );
    let all = 0..text.len_chars();
    assert_eq!(
        buf.syntax.highlights(text, all.clone()),
        fresh.highlights(text, all)
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn language_server_ui_commands_are_refused_through_the_api() {
    let (mut h, _) = Harness::with_text("fn a() {}\n");
    for name in ["goto_definition", "goto_references", "hover"] {
        let (code, msg, _) = h.fails("command.run", json!({"name": name}));
        assert_eq!(code, ErrorCode::CommandFailed, "{name}");
        assert!(msg.contains("was not run"), "{name}: {msg}");
    }
    // Ordinary commands still run afterwards.
    h.ok("command.run", json!({"name": "move_char_right"}));
}
