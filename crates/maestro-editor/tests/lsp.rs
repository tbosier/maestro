//! Language server behavior tests: keys in through `Editor::handle_key`,
//! a real child process (the fake server) on the other end.
//!
//! `harness = false`: started with `--maestro-fake-lsp`, this binary is the
//! fake server instead (see `maestro_lsp::fake`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use maestro_config::{Config, LanguageServerConfig};
use maestro_editor::lsp::{self, DiagnosticSeverity};
use maestro_editor::{Editor, KeyCode, KeyEvent, Rect};
use maestro_lsp::fake::{self, FAKE_ARG};
use maestro_lsp::{Value, json};

fn main() {
    fake::run_if_requested();
    fake::run_tests(&[
        (
            "diagnostics_show_in_gutter_status_and_pickers",
            diagnostics_show_in_gutter_status_and_pickers,
        ),
        (
            "goto_definition_moves_the_cursor",
            goto_definition_moves_the_cursor,
        ),
        (
            "stale_definition_reply_is_dropped",
            stale_definition_reply_is_dropped,
        ),
        (
            "incremental_sync_matches_after_edits_and_undo",
            incremental_sync_matches_after_edits_and_undo,
        ),
        (
            "server_crash_leaves_editor_usable",
            server_crash_leaves_editor_usable,
        ),
        (
            "missing_server_is_reported_once",
            missing_server_is_reported_once,
        ),
        (
            "references_picker_and_hover_popup",
            references_picker_and_hover_popup,
        ),
        (
            "save_and_close_reach_the_server",
            save_and_close_reach_the_server,
        ),
        ("diagnostics_follow_edits", diagnostics_follow_edits),
    ]);
}

const WAIT: Duration = Duration::from_secs(10);

/// Compact key script, as in `editing.rs`: `<...>` holds key notation.
fn keys(script: &str) -> Vec<KeyEvent> {
    let mut out = Vec::new();
    let mut chars = script.chars();
    while let Some(c) = chars.next() {
        if c == '<' {
            let name: String = chars.by_ref().take_while(|&c| c != '>').collect();
            out.push(name.parse().unwrap_or_else(|_| panic!("bad key {name}")));
        } else if c == '\n' {
            out.push(KeyEvent::plain(KeyCode::Enter));
        } else {
            out.push(KeyEvent::char(c));
        }
    }
    out
}

fn run(ed: &mut Editor, script: &str) {
    ed.feed_keys(&keys(script));
}

fn fake_config() -> Config {
    let mut config = Config::default();
    config.language_server.insert(
        "rust".into(),
        LanguageServerConfig {
            command: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args: vec![FAKE_ARG.into()],
            enabled: true,
        },
    );
    config
}

struct Fixture {
    ed: Editor,
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("maestro-lsp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An editor with language servers on, showing `main.rs` with `text`, and
/// the document already open on the fake server.
fn open(name: &str, text: &str, config: Config) -> Fixture {
    let dir = temp_dir(name);
    let path = dir.join("main.rs");
    std::fs::write(&path, text).unwrap();
    let mut ed = Editor::new(config, Rect::new(0, 0, 100, 30));
    ed.lsp.enabled = true;
    ed.open(&path).unwrap();
    Fixture { ed, dir }
}

fn open_synced(name: &str, text: &str) -> Fixture {
    let mut f = open(name, text, fake_config());
    let doc = f.ed.view().doc;
    assert!(
        wait_until(&mut f.ed, |ed| ed.lsp.is_synced(doc)),
        "server never opened the document; status {:?}",
        f.ed.status
    );
    f
}

/// Runs background jobs until `cond` holds or the wait times out.
fn wait_until(ed: &mut Editor, mut cond: impl FnMut(&Editor) -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    loop {
        ed.poll_background();
        if cond(ed) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Asks the fake server for its copy of the focused document.
fn server_doc(ed: &Editor) -> Value {
    let rx = ed
        .lsp
        .raw_request(ed.view().doc, "fake/documentText", json!({}))
        .expect("document is synced");
    maestro_lsp::client::recv_reply(&rx, WAIT)
        .expect("reply")
        .expect("ok")
}

fn assert_server_matches(ed: &Editor, step: &str) {
    let got = server_doc(ed);
    assert_eq!(got["errors"], json!([]), "after {step}");
    assert_eq!(
        got["text"].as_str(),
        Some(ed.doc().text().to_string().as_str()),
        "after {step}"
    );
    assert_eq!(
        got["version"].as_u64(),
        Some(ed.doc().version().0),
        "after {step}"
    );
}

fn head(ed: &Editor) -> usize {
    ed.view().selection.primary().head
}

fn status(ed: &Editor) -> String {
    ed.status
        .as_ref()
        .map(|(_, m)| m.clone())
        .unwrap_or_default()
}

fn diagnostics_show_in_gutter_status_and_pickers() {
    let mut f = open_synced(
        "diag",
        "fn main() {}\nlet x = ERROR;\n// WARNING and HINT\n",
    );
    let ed = &mut f.ed;
    let doc = ed.view().doc;
    assert!(wait_until(ed, |ed| ed.lsp.diagnostics(doc).len() == 3));

    // Sign column: the most severe diagnostic starting on each line.
    let signs = lsp::line_signs(ed, doc, 0..10);
    assert_eq!(signs.get(&0), None);
    assert_eq!(signs.get(&1), Some(&DiagnosticSeverity::Error));
    assert_eq!(signs.get(&2), Some(&DiagnosticSeverity::Warning));
    assert_eq!(DiagnosticSeverity::Error.letter(), 'E');

    // The idle message follows the cursor line.
    assert!(lsp::cursor_diagnostic(ed).is_none());
    run(ed, "j");
    assert_eq!(lsp::cursor_diagnostic(ed).unwrap().message, "found ERROR");
    run(ed, "j");
    assert_eq!(lsp::cursor_diagnostic(ed).unwrap().message, "found WARNING");

    // ]d and [d select diagnostic ranges, wrapping at the ends.
    run(ed, "gg]d");
    let r = ed.view().selection.primary();
    assert_eq!((r.from(), r.head), (21, 25), "ERROR is chars 21..26");
    run(ed, "]d]d");
    assert!(status(ed).contains("found HINT"), "{}", status(ed));
    run(ed, "]d");
    assert_eq!(
        ed.view().selection.primary().from(),
        21,
        "wrapped to the first"
    );
    run(ed, "[d");
    assert!(status(ed).contains("found HINT"));

    // space d lists this buffer's diagnostics; Enter jumps.
    run(ed, "gg<space>d");
    let picker = ed.picker.as_ref().expect("diagnostics picker");
    assert_eq!(picker.total(), 3);
    assert!(
        picker.items()[0].label.starts_with("E 2:9"),
        "{:?}",
        picker.items()[0]
    );
    run(ed, "WARN<ret>");
    assert!(ed.picker.is_none());
    let text = ed.doc().text().clone();
    assert_eq!(text.char_to_line(head(ed)), 2);

    // space D lists every buffer's.
    run(ed, "<space>D");
    assert_eq!(ed.picker.as_ref().unwrap().total(), 3);
    assert!(
        ed.picker.as_ref().unwrap().items()[0]
            .label
            .contains("main.rs:2:9")
    );
    run(ed, "<esc>");
}

fn goto_definition_moves_the_cursor() {
    let mut f = open_synced("def", "fn helper() {}\nfn main() {\n    helper();\n}\n");
    let ed = &mut f.ed;
    run(ed, "3G^");
    assert_eq!(head(ed), 31, "cursor on helper() in main");
    run(ed, "gd");
    assert!(
        wait_until(ed, |ed| head(ed) == 3),
        "cursor at {}; status {:?}",
        head(ed),
        ed.status
    );
    // The jump list returns to the call.
    run(ed, "<C-o>");
    assert_eq!(ed.view().selection.primary().from(), 31);
}

fn stale_definition_reply_is_dropped() {
    // `@slow` makes the fake server wait before answering.
    let mut f = open_synced("stale", "// @slow\nfn helper() {}\nhelper();\n");
    let ed = &mut f.ed;
    run(ed, "3G");
    let before = head(ed);
    run(ed, "gd");
    // Edit while the request is out.
    run(ed, "Ax<esc>");
    let edited = ed.doc().text().to_string();
    assert!(
        wait_until(ed, |ed| status(ed).contains("definition result dropped")),
        "status {:?}",
        ed.status
    );
    assert_eq!(ed.doc().text().to_string(), edited);
    assert_ne!(head(ed), 12, "did not jump to the stale definition");
    assert!(ed.doc().text().char_to_line(head(ed)) == ed.doc().text().char_to_line(before));
    // A fresh request after the edit works.
    run(ed, "0gd");
    assert!(
        wait_until(ed, |ed| head(ed) == 12),
        "status {:?}",
        ed.status
    );
}

fn incremental_sync_matches_after_edits_and_undo() {
    let mut f = open_synced("sync", "a😀b\nfoo 🚀 foo\nlast foo\n");
    let ed = &mut f.ed;
    assert_server_matches(ed, "open");
    for (step, script) in [
        ("insert", "ihello 😀<esc>"),
        ("open below", "ojust a line<esc>"),
        ("multi-cursor change", "%sfoo<ret>cbär<esc>"),
        ("undo", "u"),
        ("undo again", "u"),
        ("redo", "U"),
        ("delete lines", "ggxd"),
        ("paste", "p"),
        ("join", "%J"),
        ("undo join", "u"),
        ("repeat", "gg."),
    ] {
        run(ed, script);
        assert_server_matches(ed, step);
    }
    // A reload replaces the whole text through the same hook.
    run(ed, ":reload!<ret>");
    assert_server_matches(ed, "reload");
}

fn server_crash_leaves_editor_usable() {
    let mut f = open_synced("crash", "fn main() {}\n");
    let ed = &mut f.ed;
    run(ed, "i@crash <esc>");
    assert!(
        wait_until(ed, |ed| status(ed).contains("stopped")),
        "status {:?}",
        ed.status
    );
    assert!(status(ed).contains("exit code 3"), "{}", status(ed));
    assert!(status(ed).contains(":lsp-restart"));
    let doc = ed.view().doc;
    assert!(!ed.lsp.is_synced(doc));
    // Editing goes on, and the crashed server is not restarted on its own.
    run(ed, "Aok<esc>");
    assert_eq!(ed.doc().text().to_string(), "@crash fn main() {}ok\n");
    std::thread::sleep(Duration::from_millis(200));
    ed.poll_background();
    ed.tick(Instant::now());
    assert!(ed.lsp.server_names().is_empty());
    run(ed, "gd");
    assert!(status(ed).contains("not running"), "{}", status(ed));
    // Remove the trigger and restart.
    run(ed, "uu:lsp-restart<ret>");
    assert!(!ed.doc().text().to_string().contains("@crash"));
    assert!(
        wait_until(ed, |ed| ed.lsp.is_synced(doc)),
        "status {:?}",
        ed.status
    );
    assert_server_matches(ed, "restart");
}

fn missing_server_is_reported_once() {
    let mut config = Config::default();
    config.language_server.insert(
        "rust".into(),
        LanguageServerConfig::new("maestro-no-such-language-server", &[]),
    );
    let mut f = open("missing", "fn main() {}\n", config);
    let ed = &mut f.ed;
    assert!(
        status(ed).contains("maestro-no-such-language-server not found on PATH"),
        "{}",
        status(ed)
    );
    for _ in 0..3 {
        run(ed, "j");
        ed.tick(Instant::now());
        assert!(!status(ed).contains("not found"), "reported again");
    }
    run(ed, "gd");
    assert!(status(ed).contains("not running"), "{}", status(ed));
    assert!(ed.lsp.server_names().is_empty());
}

fn references_picker_and_hover_popup() {
    let mut f = open_synced(
        "refs",
        "fn helper() {}\nfn main() {\n    helper();\n    helper();\n}\n",
    );
    let ed = &mut f.ed;
    run(ed, "3G^");
    run(ed, "gr");
    assert!(
        wait_until(ed, |ed| ed.picker.is_some()),
        "status {:?}",
        ed.status
    );
    let picker = ed.picker.as_ref().unwrap();
    assert_eq!(picker.title, "references");
    assert_eq!(picker.total(), 3);
    assert!(
        picker.items()[2].label.ends_with("main.rs:4:5  helper();"),
        "{:?}",
        picker.items()[2]
    );
    run(ed, "<down><down><ret>");
    assert_eq!(head(ed), 45, "on the second call");

    run(ed, "<space>k");
    assert!(
        wait_until(ed, |ed| ed.lsp.hover.is_some()),
        "status {:?}",
        ed.status
    );
    let hover = ed.lsp.hover.clone().unwrap();
    assert_eq!(
        hover.lines,
        ["fn helper()", "", "Documentation for `helper`."]
    );
    run(ed, "l");
    assert!(ed.lsp.hover.is_none(), "any key closes the popup");
}

fn save_and_close_reach_the_server() {
    let mut f = open_synced("save", "fn main() {}\n");
    let other = f.dir.join("other.rs");
    std::fs::write(&other, "fn other() {}\n").unwrap();
    let ed = &mut f.ed;
    let first = ed.view().doc;
    run(ed, "ix<esc>:w<ret>");
    assert!(!ed.doc().is_modified(), "{:?}", ed.status);
    ed.open(&other).unwrap();
    let second = ed.view().doc;
    assert!(wait_until(ed, |ed| ed.lsp.is_synced(second)));
    run(ed, ":bc<ret>");
    assert_eq!(ed.view().doc, first, "closing other.rs shows main.rs again");
    let rx = ed
        .lsp
        .raw_request(first, "fake/stats", json!({}))
        .expect("synced");
    let stats = maestro_lsp::client::recv_reply(&rx, WAIT).unwrap().unwrap();
    let methods: Vec<&str> = stats["received"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        [
            "initialized",
            "textDocument/didOpen",
            "textDocument/didChange",
            "textDocument/didSave",
            "textDocument/didOpen",
            "textDocument/didClose",
        ]
    );
    let closed = &stats["received"][5]["params"]["textDocument"]["uri"];
    assert!(closed.as_str().unwrap().ends_with("/other.rs"));
}

fn diagnostics_follow_edits() {
    let mut f = open_synced("follow", "x ERROR\n");
    let ed = &mut f.ed;
    let doc = ed.view().doc;
    assert!(wait_until(ed, |ed| ed.lsp.diagnostics(doc).len() == 1));
    // Insert before the diagnostic: until the server republishes, the
    // stored range moves with the text.
    run(ed, "iab<esc>");
    let d = &ed.lsp.diagnostics(doc)[0];
    let text = ed.doc().text().to_string();
    assert_eq!(&text[d.from..d.to], "ERROR");
    assert!(wait_until(ed, |ed| {
        ed.lsp.diagnostics(doc).first().map(|d| d.from) == Some(4)
    }));
}
