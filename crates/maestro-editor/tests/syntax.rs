//! Syntax highlighting and structural selection, driven through keys.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use maestro_config::Config;
use maestro_editor::{Editor, KeyCode, KeyEvent, Rect, Severity};
use maestro_syntax::{Language, Syntax};

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

/// A fresh directory per test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("maestro-syntax-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Applies background results (parses, query compilation) until none are
/// left, as the front end's loop does between frames.
fn settle(ed: &mut Editor) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let ran = ed.poll_background();
        if !ran && !ed.has_background_work() && !ed.poll_background() {
            return;
        }
        assert!(Instant::now() < deadline, "background work finishes");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn open(path: &Path, content: &str) -> Editor {
    std::fs::write(path, content).unwrap();
    let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 100, 30));
    ed.open(path).unwrap();
    settle(&mut ed);
    ed
}

fn selected(ed: &Editor) -> String {
    let r = ed.view().selection.primary();
    r.fragment(ed.doc().text().slice(..))
}

/// Once background work settles, the buffer's tree and highlights equal a
/// fresh parse of its current text.
fn assert_in_sync(ed: &mut Editor, step: &str) {
    settle(ed);
    let buf = ed.buffer();
    let text = buf.doc.text();
    let lang = buf.syntax.language().expect("language detected");
    let syntax = buf.syntax.syntax().expect("tree present");
    let fresh = Syntax::parse(lang, text).unwrap();
    assert_eq!(
        syntax.tree().root_node().to_sexp(),
        fresh.tree().root_node().to_sexp(),
        "tree after {step}"
    );
    let all = 0..text.len_chars();
    let got = buf.syntax.highlights(text, all.clone());
    assert!(!got.is_empty(), "highlights after {step}");
    assert_eq!(got, fresh.highlights(text, all), "highlights after {step}");
}

fn scope_of(ed: &Editor, needle: &str) -> Option<&'static str> {
    let text = ed.doc().text();
    let s = text.to_string();
    let byte = s.find(needle).expect("needle in text");
    let at = text.byte_to_char(byte);
    ed.buffer()
        .syntax
        .highlights(text, 0..text.len_chars())
        .into_iter()
        .find(|(a, b, _)| *a <= at && at < *b)
        .map(|(_, _, s)| s)
}

const SRC: &str = "fn main() {\n    let total = add(1, 2);\n    report(total);\n}\n";

#[test]
fn opening_a_rust_file_highlights_it() {
    let dir = temp_dir("open");
    let mut ed = open(&dir.join("main.rs"), SRC);
    assert_eq!(ed.buffer().syntax.language(), Some(Language::Rust));
    assert_eq!(scope_of(&ed, "fn"), Some("keyword"));
    assert_eq!(scope_of(&ed, "let"), Some("keyword"));
    assert_eq!(scope_of(&ed, "add"), Some("function"));
    assert_in_sync(&mut ed, "open");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn expand_twice_selects_expression_then_statement_and_shrink_walks_back() {
    let dir = temp_dir("expand");
    let mut ed = open(&dir.join("main.rs"), SRC);
    run(&mut ed, "/add<ret>");
    assert_eq!(selected(&ed), "add");
    run(&mut ed, "<A-o>");
    assert_eq!(selected(&ed), "add(1, 2)");
    run(&mut ed, "<A-o>");
    assert_eq!(selected(&ed), "let total = add(1, 2);");
    run(&mut ed, "<A-i>");
    assert_eq!(selected(&ed), "add(1, 2)");
    run(&mut ed, "<A-i>");
    assert_eq!(selected(&ed), "add");

    // Siblings at statement level.
    run(&mut ed, "<A-o><A-o><A-n>");
    assert_eq!(selected(&ed), "report(total);");
    run(&mut ed, "<A-p>");
    assert_eq!(selected(&ed), "let total = add(1, 2);");

    // After moving away, A-i no longer replays history; it shrinks to the
    // first child node instead.
    run(&mut ed, "<A-o>");
    assert_eq!(
        selected(&ed),
        "{\n    let total = add(1, 2);\n    report(total);\n}"
    );
    run(&mut ed, "<A-p>");
    assert_eq!(selected(&ed), "()");
    run(&mut ed, "<A-i>");
    assert_eq!(selected(&ed), "()", "no named child inside ()");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn structural_commands_report_when_there_is_no_tree() {
    let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 80, 24));
    run(&mut ed, "ihello<esc>");
    let before = ed.view().selection.clone();
    run(&mut ed, "<A-o>");
    let (severity, msg) = ed.status.clone().expect("status set");
    assert_eq!(severity, Severity::Warning);
    assert!(msg.contains("not recognized"), "{msg}");
    assert_eq!(ed.view().selection, before);
}

#[test]
fn edits_undo_redo_paste_and_reload_keep_the_tree_in_sync() {
    let dir = temp_dir("edits");
    let path = dir.join("main.rs");
    let mut ed = open(&path, SRC);

    // Insert-mode typing with multibyte text, across lines.
    run(&mut ed, "/total<ret>c日本é<esc>");
    assert_in_sync(&mut ed, "change");
    run(&mut ed, "o    let s = \"漢字 é\";<esc>");
    assert_in_sync(&mut ed, "open below");
    assert_eq!(scope_of(&ed, "\"漢字"), Some("string"));
    // Multiple cursors: select every `e` in the file and delete them.
    run(&mut ed, "%se<ret>d");
    assert_in_sync(&mut ed, "multi-cursor delete");

    // Undo everything back to the file contents, then redo.
    run(&mut ed, "uuu");
    assert_eq!(ed.doc().text().to_string(), SRC);
    assert_in_sync(&mut ed, "undo");
    assert_eq!(scope_of(&ed, "add"), Some("function"));
    run(&mut ed, "UU");
    assert_in_sync(&mut ed, "redo");

    ed.paste_text("// pasted 👍\n");
    assert_in_sync(&mut ed, "paste");

    // Reload from disk replaces the whole text.
    std::fs::write(&path, "struct S;\nfn other() -> S { S }\n").unwrap();
    run(&mut ed, ":reload!<ret>");
    assert_eq!(
        ed.doc().text().to_string(),
        "struct S;\nfn other() -> S { S }\n"
    );
    assert_in_sync(&mut ed, "reload");
    assert_eq!(scope_of(&ed, "other"), Some("function"));
    run(&mut ed, "u");
    assert_in_sync(&mut ed, "undo reload");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn recover_keeps_the_tree_in_sync() {
    let dir = temp_dir("recover");
    let rec = dir.join("recovery");
    let path = dir.join("lib.rs");
    std::fs::write(&path, "fn a() {}\n").unwrap();
    maestro_editor::recovery::write(&rec, &path, &"fn b(x: u8) -> u8 { x }\n".into()).unwrap();
    let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 80, 24));
    ed.recovery_dir = Some(rec);
    ed.open(&path).unwrap();
    run(&mut ed, ":recover<ret>");
    assert_eq!(ed.doc().text().to_string(), "fn b(x: u8) -> u8 { x }\n");
    assert_in_sync(&mut ed, "recover");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn saving_under_a_new_name_detects_the_language() {
    let dir = temp_dir("saveas");
    let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 80, 24));
    run(&mut ed, "idef f(x):<ret>return x<esc>");
    assert_eq!(ed.buffer().syntax.language(), None);
    let target = dir.join("f.py");
    run(&mut ed, &format!(":w {}<ret>", target.display()));
    assert_eq!(ed.buffer().syntax.language(), Some(Language::Python));
    assert_in_sync(&mut ed, "save as");
    assert_eq!(scope_of(&ed, "def"), Some("keyword"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn typing_or_removing_a_shebang_switches_the_language() {
    let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 80, 24));
    run(
        &mut ed,
        "i#!/usr/bin/env python3<ret>def f(x):<ret>return x<esc>",
    );
    assert_eq!(ed.buffer().syntax.language(), Some(Language::Python));
    assert_in_sync(&mut ed, "shebang typed");
    assert_eq!(scope_of(&ed, "def"), Some("keyword"));
    // Delete the first line: no shebang, no language.
    run(&mut ed, "ggxd");
    assert_eq!(ed.doc().text().line(0).to_string(), "def f(x):\n");
    assert_eq!(ed.buffer().syntax.language(), None);
    assert_eq!(scope_of(&ed, "def"), None);
}

#[test]
fn a_huge_paste_reparses_from_scratch_and_stays_in_sync() {
    let dir = temp_dir("paste");
    let mut ed = open(&dir.join("main.rs"), SRC);
    let big: String = (0..8000)
        .map(|i| format!("fn pasted_{i}() -> &'static str {{ \"é漢 {i}\" }}\n"))
        .collect();
    assert!(big.len() > maestro_editor::syntax::BIG_EDIT_BYTES);
    ed.paste_text(&big);
    assert_in_sync(&mut ed, "huge paste");
    run(&mut ed, "u");
    assert_eq!(ed.doc().text().to_string(), SRC);
    assert_in_sync(&mut ed, "undo huge paste");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_files_parse_in_the_background_and_replay_edits() {
    let dir = temp_dir("large");
    let path = dir.join("big.rs");
    let mut src = String::new();
    let mut i = 0;
    while src.len() < 2 * maestro_editor::syntax::SYNC_PARSE_BYTES {
        src.push_str(&format!(
            "fn item_{i}(x: u32) -> u32 {{ let y = x + {i}; y * 2 }}\n"
        ));
        i += 1;
    }
    std::fs::write(&path, &src).unwrap();
    let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 100, 30));
    ed.open(&path).unwrap();
    assert!(ed.buffer().syntax.is_parsing());
    assert!(ed.buffer().syntax.syntax().is_none());
    // Highlights are simply absent while parsing; commands explain why.
    run(&mut ed, "<A-o>");
    assert!(ed.status.as_ref().unwrap().1.contains("still being built"));

    // Edit while the parse runs; the edits must be replayed onto the tree.
    run(&mut ed, "ifn é() {}<ret><esc>jxd");
    settle(&mut ed);
    assert!(!ed.buffer().syntax.is_parsing());
    assert_in_sync(&mut ed, "background parse with replayed edits");
    run(&mut ed, "u");
    assert_in_sync(&mut ed, "undo after background parse");
    std::fs::remove_dir_all(&dir).unwrap();
}
