//! End-to-end editing tests: key sequences in, text and selections out.

use maestro_config::{Config, Profile};
use maestro_core::{Document, Range};
use maestro_editor::{Editor, KeyCode, KeyEvent, Mode, Rect};

/// Parses a compact key script. Plain characters are typed as-is; `<...>`
/// holds key notation, e.g. `<esc>`, `<C-r>`, `<ret>`, `<A-;>`, `<space>`.
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

fn editor_with(text: &str, profile: Profile) -> Editor {
    let config = Config {
        profile,
        ..Config::default()
    };
    let mut ed = Editor::new(config, Rect::new(0, 0, 80, 24));
    let id = ed.add_document(Document::from_text(text));
    ed.switch_to(id);
    ed
}

fn editor(text: &str) -> Editor {
    editor_with(text, Profile::Maestro)
}

fn run(ed: &mut Editor, script: &str) {
    ed.feed_keys(&keys(script));
}

fn text(ed: &Editor) -> String {
    ed.doc().text().to_string()
}

fn ranges(ed: &Editor) -> Vec<(usize, usize)> {
    ed.view()
        .selection
        .iter()
        .map(|r| (r.anchor, r.head))
        .collect()
}

#[test]
fn insert_undo_redo() {
    let mut ed = editor("fn main() {}\n");
    run(&mut ed, "ihello <esc>");
    assert_eq!(text(&ed), "hello fn main() {}\n");
    assert_eq!(ed.mode, Mode::Normal);
    run(&mut ed, "u");
    assert_eq!(text(&ed), "fn main() {}\n");
    assert!(!ed.doc().is_modified());
    run(&mut ed, "U");
    assert_eq!(text(&ed), "hello fn main() {}\n");
    run(&mut ed, "u<C-r>");
    assert_eq!(text(&ed), "hello fn main() {}\n");
}

#[test]
fn whole_insert_session_is_one_undo_step() {
    let mut ed = editor("");
    run(&mut ed, "ione\ntwo\nthree<esc>");
    assert_eq!(text(&ed), "one\ntwo\nthree");
    run(&mut ed, "u");
    assert_eq!(text(&ed), "");
}

#[test]
fn word_selection_and_delete() {
    let mut ed = editor("alpha beta gamma\n");
    run(&mut ed, "wd");
    assert_eq!(text(&ed), "beta gamma\n");
    run(&mut ed, "ed");
    assert_eq!(text(&ed), " gamma\n");
}

#[test]
fn change_and_dot_repeat() {
    let mut ed = editor("aaa bbb ccc\n");
    run(&mut ed, "ecX<esc>");
    assert_eq!(text(&ed), "X bbb ccc\n");
    run(&mut ed, "w");
    run(&mut ed, "w.");
    // `.` replays "ecX<esc>" from the new position.
    assert_eq!(text(&ed), "X bbb X\n");
}

#[test]
fn multiple_cursors_via_regex_select() {
    let mut ed = editor("let a = 1;\nlet b = 2;\nlet c = 3;\n");
    run(&mut ed, "%slet<ret>");
    assert_eq!(ed.view().selection.len(), 3);
    run(&mut ed, "cconst<esc>");
    assert_eq!(text(&ed), "const a = 1;\nconst b = 2;\nconst c = 3;\n");
    run(&mut ed, "u");
    assert_eq!(text(&ed), "let a = 1;\nlet b = 2;\nlet c = 3;\n");
}

#[test]
fn copy_selection_down_and_insert() {
    let mut ed = editor("abc\ndef\nghi\n");
    run(&mut ed, "CC");
    assert_eq!(ed.view().selection.len(), 3);
    run(&mut ed, "i-<esc>");
    assert_eq!(text(&ed), "-abc\n-def\n-ghi\n");
    run(&mut ed, ",");
    assert_eq!(ed.view().selection.len(), 1);
}

#[test]
fn line_select_yank_paste() {
    let mut ed = editor("one\ntwo\n");
    run(&mut ed, "xyp");
    assert_eq!(text(&ed), "one\none\ntwo\n");
    run(&mut ed, "jxd");
    assert_eq!(text(&ed), "one\none\n");
}

#[test]
fn counts_and_goto() {
    let mut ed = editor("1\n2\n3\n4\n5\n");
    run(&mut ed, "3j");
    assert_eq!(ed.primary().head, 6);
    run(&mut ed, "gg");
    assert_eq!(ed.primary().head, 0);
    run(&mut ed, "G");
    assert_eq!(ed.primary().head, 8);
    run(&mut ed, "2G");
    assert_eq!(ed.primary().head, 2);
    run(&mut ed, ":4<ret>");
    assert_eq!(ed.primary().head, 6);
}

#[test]
fn search_next_and_wrap() {
    let mut ed = editor("foo bar foo baz foo\n");
    run(&mut ed, "/foo<ret>");
    assert_eq!(ranges(&ed), vec![(8, 10)]);
    run(&mut ed, "n");
    assert_eq!(ranges(&ed), vec![(16, 18)]);
    run(&mut ed, "n");
    assert_eq!(ranges(&ed), vec![(0, 2)]);
    run(&mut ed, "N");
    assert_eq!(ranges(&ed), vec![(16, 18)]);
}

#[test]
fn search_escape_restores_selection() {
    let mut ed = editor("abc xyz abc\n");
    run(&mut ed, "/xyz");
    assert_eq!(ranges(&ed), vec![(4, 6)]);
    run(&mut ed, "<esc>");
    assert_eq!(ranges(&ed), vec![(0, 0)]);
}

#[test]
fn substitute_whole_file_and_selected_lines() {
    let mut ed = editor("a-a\nb-b\na-a\n");
    run(&mut ed, ":%s/a/X/g<ret>");
    assert_eq!(text(&ed), "X-X\nb-b\nX-X\n");
    run(&mut ed, "u");
    run(&mut ed, ":s/a/Y/<ret>");
    assert_eq!(text(&ed), "Y-a\nb-b\na-a\n");
    run(&mut ed, ":%s/(\\w)-(\\w)/\\2+\\1/<ret>");
    assert_eq!(text(&ed), "a+Y\nb+b\na+a\n");
}

#[test]
fn surround_and_text_objects() {
    let mut ed = editor("call(a, b)\n");
    run(&mut ed, "f(mi(d");
    assert_eq!(text(&ed), "call()\n");
    run(&mut ed, "u");
    run(&mut ed, "mr([");
    assert_eq!(text(&ed), "call[a, b]\n");
    run(&mut ed, "md[");
    assert_eq!(text(&ed), "calla, b\n");
    run(&mut ed, "gg");
    run(&mut ed, "ems\"");
    assert_eq!(text(&ed), "\"calla\", b\n");
}

#[test]
fn indent_newline_and_brackets() {
    let mut ed = editor("fn f() {}\n");
    run(&mut ed, "f{a\nx<esc>");
    assert_eq!(text(&ed), "fn f() {\n    x\n}\n");
    run(&mut ed, "x>");
    assert_eq!(text(&ed), "fn f() {\n        x\n}\n");
    run(&mut ed, "<lt>");
    assert_eq!(text(&ed), "fn f() {\n    x\n}\n");
}

#[test]
fn open_lines_and_backspace_to_indent_stop() {
    let mut ed = editor("    a\n");
    run(&mut ed, "ob<esc>");
    assert_eq!(text(&ed), "    a\n    b\n");
    run(&mut ed, "O<backspace>c<esc>");
    assert_eq!(text(&ed), "    a\nc\n    b\n");
}

#[test]
fn macros_record_and_replay() {
    let mut ed = editor("a\nb\nc\n");
    run(&mut ed, "QI- <esc>jQ");
    assert_eq!(text(&ed), "- a\nb\nc\n");
    run(&mut ed, "2q");
    assert_eq!(text(&ed), "- a\n- b\n- c\n");
}

#[test]
fn registers_and_paste_per_selection() {
    let mut ed = editor("one two\n");
    run(&mut ed, "w\"ay\"ap");
    assert_eq!(text(&ed), "one one two\n");
    run(&mut ed, "u");
    run(&mut ed, "%s\\w+<ret>yP");
    assert_eq!(text(&ed), "oneone twotwo\n");
}

#[test]
fn splits_and_views_share_documents() {
    let mut ed = editor("shared\n");
    run(&mut ed, "<C-w>v");
    assert_eq!(ed.layout.views().len(), 2);
    run(&mut ed, "iX<esc>");
    let doc = ed.view().doc;
    // The other view sees the same text and its cursor was mapped.
    let other = ed
        .views
        .values()
        .find(|v| v.id != ed.focus)
        .expect("second view");
    assert_eq!(other.doc, doc);
    assert_eq!(text(&ed), "Xshared\n");
    run(&mut ed, ":q<ret>");
    assert_eq!(ed.layout.views().len(), 1);
    assert!(!ed.should_quit);
}

#[test]
fn quit_refuses_with_unsaved_changes() {
    let mut ed = editor("x\n");
    run(&mut ed, "iy<esc>:q<ret>");
    assert!(!ed.should_quit);
    assert!(ed.status.as_ref().unwrap().1.contains("unsaved"));
    run(&mut ed, ":q!<ret>");
    assert!(ed.should_quit);
}

#[test]
fn replace_case_and_join() {
    let mut ed = editor("hello\n  world\n");
    run(&mut ed, "wrX");
    assert_eq!(text(&ed), "XXXXX\n  world\n");
    run(&mut ed, "u~");
    assert_eq!(text(&ed), "HELLO\n  world\n");
    run(&mut ed, "J");
    assert_eq!(text(&ed), "HELLO world\n");
}

#[test]
fn select_mode_extends() {
    let mut ed = editor("abcdef\n");
    run(&mut ed, "vlll");
    assert_eq!(ranges(&ed), vec![(0, 3)]);
    run(&mut ed, "<esc>d");
    assert_eq!(text(&ed), "ef\n");
}

#[test]
fn unicode_graphemes_move_as_units() {
    let mut ed = editor("e\u{301}x👨\u{200d}👩\u{200d}👧y\n");
    run(&mut ed, "ld");
    assert_eq!(text(&ed), "e\u{301}👨\u{200d}👩\u{200d}👧y\n");
    run(&mut ed, "d");
    assert_eq!(text(&ed), "e\u{301}y\n");
    run(&mut ed, "hd");
    assert_eq!(text(&ed), "y\n");
}

#[test]
fn crlf_is_preserved_on_new_lines() {
    let mut ed = editor("a\r\nb\r\n");
    run(&mut ed, "onew<esc>");
    assert_eq!(text(&ed), "a\r\nnew\r\nb\r\n");
}

#[test]
fn vim_profile_basics() {
    let mut ed = editor_with("one\ntwo\nthree\n", Profile::Vim);
    run(&mut ed, "ddp");
    assert_eq!(text(&ed), "two\none\nthree\n");
    run(&mut ed, "x");
    assert_eq!(text(&ed), "two\nne\nthree\n");
    run(&mut ed, "D");
    assert_eq!(text(&ed), "two\n\nthree\n");
}

#[test]
fn unknown_keys_report_instead_of_acting() {
    let mut ed = editor("abc\n");
    run(&mut ed, "gZ");
    assert_eq!(text(&ed), "abc\n");
    assert!(ed.status.as_ref().unwrap().1.contains("no binding"));
}

#[test]
fn read_only_buffers_refuse_edits() {
    let dir = std::env::temp_dir().join(format!("maestro-ro-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bin.dat");
    std::fs::write(&path, b"ab\x00cd").unwrap();
    let mut ed = editor("");
    ed.open(&path).unwrap();
    run(&mut ed, "ix<esc>");
    assert_eq!(ed.doc().text().len_chars(), 5);
    assert_eq!(std::fs::read(&path).unwrap(), b"ab\x00cd");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn m0_acceptance_open_edit_undo_redo_save_reopen() {
    // The M0 acceptance flow, driven through the same key handling the
    // terminal uses: open a Rust file, change code, undo, redo, save, close,
    // reopen, verify.
    let dir = std::env::temp_dir().join(format!("maestro-m0-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.rs");
    std::fs::write(&path, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();

    let mut ed = editor("");
    ed.open(&path).unwrap();
    run(&mut ed, "jfhmi\"cbye<esc>");
    assert_eq!(text(&ed), "fn main() {\n    println!(\"bye\");\n}\n");
    run(&mut ed, "u");
    assert_eq!(text(&ed), "fn main() {\n    println!(\"hi\");\n}\n");
    run(&mut ed, "U");
    run(&mut ed, ":w<ret>");
    assert!(!ed.doc().is_modified(), "status: {:?}", ed.status);
    run(&mut ed, ":q<ret>");
    assert!(ed.should_quit);

    let mut ed2 = editor("");
    ed2.open(&path).unwrap();
    assert_eq!(text(&ed2), "fn main() {\n    println!(\"bye\");\n}\n");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "fn main() {\n    println!(\"bye\");\n}\n"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn recovery_files_follow_modifications() {
    let dir = std::env::temp_dir().join(format!("maestro-rec-{}", std::process::id()));
    let rec = dir.join("recovery");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.txt");
    std::fs::write(&path, "base\n").unwrap();
    let mut ed = editor("");
    ed.recovery_dir = Some(rec.clone());
    ed.config.editor.recovery_interval_secs = 1;
    ed.open(&path).unwrap();
    run(&mut ed, "iedit <esc>");
    let later = std::time::Instant::now() + std::time::Duration::from_secs(5);
    ed.tick(later);
    assert_eq!(
        maestro_editor::recovery::read(&rec, &path).as_deref(),
        Some("edit base\n")
    );
    // A new session announces it and :recover restores it.
    let mut ed2 = editor("");
    ed2.recovery_dir = Some(rec.clone());
    ed2.open(&path).unwrap();
    assert!(ed2.status.as_ref().unwrap().1.contains(":recover"));
    run(&mut ed2, ":recover<ret>");
    assert_eq!(text(&ed2), "edit base\n");
    run(&mut ed2, ":w<ret>");
    assert!(maestro_editor::recovery::read(&rec, &path).is_none());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn external_change_reload_when_clean_and_warn_when_dirty() {
    let dir = std::env::temp_dir().join(format!("maestro-ext-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.txt");
    std::fs::write(&path, "v1\n").unwrap();
    let mut ed = editor("");
    ed.open(&path).unwrap();
    std::fs::write(&path, "version two\n").unwrap();
    ed.tick(std::time::Instant::now());
    assert_eq!(text(&ed), "version two\n");
    run(&mut ed, "imine <esc>");
    std::fs::write(&path, "version three!\n").unwrap();
    ed.tick(std::time::Instant::now());
    assert_eq!(text(&ed), "mine version two\n");
    assert!(ed.status.as_ref().unwrap().1.contains("changed on disk"));
    run(&mut ed, ":w<ret>");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "version three!\n");
    run(&mut ed, ":w!<ret>");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "mine version two\n"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ranges_stay_valid_after_random_edits() {
    // Smoke property: arbitrary key soup never panics and leaves selections
    // inside the document.
    let soup = "wbeWBExX%;,CsC<esc>dciaoO<esc>pPuUJ~`rx>.<lt>vjkhl<esc>";
    let mut ed = editor("fn a() {\n    b(c, [d]);\n}\n\n// é 漢字 👍\n");
    for _ in 0..20 {
        run(&mut ed, soup);
        let len = ed.doc().text().len_chars();
        for r in ed.view().selection.iter() {
            assert!(r.anchor <= len && r.head <= len, "{r:?} out of {len}");
        }
    }
    let _ = Range::point(0);
}
