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
        maestro_editor::recovery::read(&rec, &path, &ed.session_id).as_deref(),
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
    // The crashed session's file was adopted and the new session saved.
    assert!(maestro_editor::recovery::all_for(&rec, &path).is_empty());
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

#[test]
fn background_jobs_apply_on_poll() {
    let mut ed = editor("x\n");
    ed.spawn_background("test", |tx| {
        tx.send(|ed| ed.info("from background"));
    })
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ed.poll_background() {
        assert!(std::time::Instant::now() < deadline, "job never arrived");
        std::thread::yield_now();
    }
    assert_eq!(ed.status.as_ref().unwrap().1, "from background");
    while ed.has_background_work() {
        assert!(std::time::Instant::now() < deadline, "job counter stuck");
        std::thread::yield_now();
    }
}

#[test]
fn sending_a_job_wakes_the_front_end() {
    let ed = editor("x\n");
    let (tx, rx) = std::sync::mpsc::channel();
    // A sender made before the waker is installed must still reach it.
    let idle = ed.idle_job_sender();
    assert!(ed.set_waker(move || {
        let _ = tx.send(());
    }));
    assert!(!ed.set_waker(|| {}), "a second waker must be refused");
    idle.send(|ed| ed.info("idle"));
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .expect("idle sender did not wake");
    ed.spawn_background("wake", |tx| {
        tx.send(|ed| ed.info("background"));
    })
    .unwrap();
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .expect("background sender did not wake");
    ed.wake_handle().wake();
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .expect("wake handle did not wake");
}

// ----- regressions from the M0 hostile review -------------------------------

#[test]
fn self_invoking_macro_stops_with_an_error() {
    let mut ed = editor("abc\n");
    // `q` while recording into @ is refused but recorded; replaying the
    // macro then calls itself.
    run(&mut ed, "QqQ");
    run(&mut ed, "q");
    assert!(ed.status.as_ref().unwrap().1.contains("macro nesting"));
    assert_eq!(text(&ed), "abc\n");
}

#[test]
fn prompt_ctrl_w_after_multibyte_space_does_not_panic() {
    let mut ed = editor("abc\n");
    run(&mut ed, ":\u{3000}x<C-w>");
    assert_eq!(ed.prompt.as_ref().unwrap().input, "\u{3000}");
    run(&mut ed, "<esc>");
}

#[test]
fn paste_into_prompt_and_picker_never_edits_the_document() {
    let mut ed = editor("hello world\n");
    run(&mut ed, "/");
    ed.paste_text("wor");
    assert_eq!(ed.prompt.as_ref().unwrap().input, "wor");
    run(&mut ed, "<ret>");
    assert_eq!(text(&ed), "hello world\n");
    assert_eq!(ranges(&ed), vec![(6, 8)]);
    run(&mut ed, "<space>?");
    ed.paste_text("undo");
    assert_eq!(ed.picker.as_ref().unwrap().query, "undo");
    run(&mut ed, "<esc>");
    assert_eq!(text(&ed), "hello world\n");
}

#[test]
fn ctrl_w_in_insert_never_splits_crlf_or_graphemes() {
    let mut ed = editor("a\r\nb\r\n");
    run(&mut ed, "ji<C-w><esc>");
    assert_eq!(text(&ed), "ab\r\n");
    let mut ed = editor("xe\u{301}\n");
    run(&mut ed, "A<C-w><esc>");
    assert_eq!(text(&ed), "\n");
}

#[test]
fn replacement_selects_the_whole_result() {
    let mut ed = editor("foo bar\n");
    run(&mut ed, "erX");
    assert_eq!(text(&ed), "XXX bar\n");
    run(&mut ed, "rY");
    assert_eq!(text(&ed), "YYY bar\n");
}

#[test]
fn linewise_paste_at_unterminated_eof_keeps_blank_lines() {
    let mut ed = editor("one\n\nx");
    run(&mut ed, "xxyGp");
    assert_eq!(text(&ed), "one\n\nx\none\n");
}

#[test]
fn earlier_reaches_an_abandoned_undo_branch() {
    let mut ed = editor("");
    run(&mut ed, "iA<esc>uiB<esc>");
    assert_eq!(text(&ed), "B");
    run(&mut ed, "uU");
    assert_eq!(text(&ed), "B");
    run(&mut ed, "g-");
    assert_eq!(text(&ed), "A");
    run(&mut ed, "g+");
    assert_eq!(text(&ed), "B");
}

#[test]
fn write_path_with_spaces_names_one_file() {
    let dir = std::env::temp_dir().join(format!("maestro-space-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut ed = editor("");
    ed.cwd = dir.clone();
    run(&mut ed, "ihi<esc>:w my file.rs<ret>");
    assert_eq!(
        std::fs::read_to_string(dir.join("my file.rs")).unwrap(),
        "hi"
    );
    assert!(!dir.join("my").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn regex_prompt_survives_a_reload_that_shrinks_the_file() {
    let dir = std::env::temp_dir().join(format!("maestro-shrink-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.txt");
    std::fs::write(&path, "abcdef").unwrap();
    let mut ed = editor("");
    ed.open(&path).unwrap();
    run(&mut ed, "5ls");
    std::fs::write(&path, "x").unwrap();
    ed.tick(std::time::Instant::now());
    assert_eq!(text(&ed), "x");
    run(&mut ed, "x<ret>");
    std::fs::remove_dir_all(&dir).unwrap();
}

// ----- regressions from the integration review ------------------------------

#[test]
fn reload_while_an_insert_mode_regex_prompt_is_open_does_not_crash() {
    let dir = std::env::temp_dir().join(format!("maestro-ins-prompt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.txt");
    std::fs::write(&path, "a long line of text\n").unwrap();
    let mut config = Config::default();
    config
        .keys
        .entry("insert".into())
        .or_default()
        .insert("C-f".into(), "select_regex".into());
    let mut ed = Editor::new(config, Rect::new(0, 0, 80, 24));
    ed.open(&path).unwrap();
    run(&mut ed, "A<C-f>");
    assert!(ed.prompt.is_some());
    std::fs::write(&path, "x\n").unwrap();
    ed.tick(std::time::Instant::now());
    assert_eq!(text(&ed), "x\n");
    run(&mut ed, "<esc>");
    run(&mut ed, "<ret>");
    assert!(ed.doc().text().len_chars() >= 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_buffer_shown_again_keeps_its_cursor_on_the_same_char_after_edits() {
    let mut ed = editor("abcdefghij\n");
    let a = ed.view().doc;
    run(&mut ed, "5l"); // on 'f'
    run(&mut ed, "<C-w>v"); // second view on the same buffer
    let b = ed.add_document(Document::from_text("other\n"));
    ed.switch_to(b); // caches A's cursor (offset 5)
    run(&mut ed, "<C-w>w"); // the other view, still on A
    run(&mut ed, "ggi\n<esc>"); // insert a line break before 'f'
    run(&mut ed, "<C-w>w");
    ed.switch_to(a);
    let p = ed.primary().head;
    assert_eq!(ed.doc().text().char(p), 'f');
}

#[test]
fn executing_a_command_line_keeps_the_normal_mode_invariant() {
    let mut ed = editor("abcdef");
    run(&mut ed, "A");
    ed.execute_line("normal_mode");
    assert_eq!(ed.mode, Mode::Normal);
    assert!(ed.primary().head < 6);
}

#[test]
fn external_reload_keeps_the_cursor_on_its_text() {
    let dir = std::env::temp_dir().join(format!("maestro-reload-keep-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.txt");
    std::fs::write(&path, "alpha\nbeta\ngamma\ndelta\n").unwrap();
    let mut ed = editor("");
    ed.open(&path).unwrap();
    run(&mut ed, "2jl"); // on the 'a' after 'g' in gamma
    std::fs::write(&path, "alpha\nbeta\ngamma\nDELTA changed\n").unwrap();
    ed.tick(std::time::Instant::now());
    assert_eq!(text(&ed), "alpha\nbeta\ngamma\nDELTA changed\n");
    assert_eq!(ed.primary().head, 12, "cursor did not move to the top");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A fresh, canonical directory per test.
fn save_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("maestro-saveas-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn status_error(ed: &Editor) -> String {
    match &ed.status {
        Some((maestro_editor::Severity::Error, msg)) => msg.clone(),
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn a_failed_write_to_a_new_path_keeps_the_buffer_as_it_was() {
    use maestro_syntax::Language;
    let dir = save_dir("missing-dir");
    let path = dir.join("a.rs");
    std::fs::write(&path, "fn a() {}\n").unwrap();
    let mut ed = editor("");
    ed.cwd = dir.clone();
    ed.open(&path).unwrap();
    run(&mut ed, "iX<esc>");
    assert!(ed.doc().is_modified());
    assert_eq!(ed.buffer().syntax.language(), Some(Language::Rust));

    // The directory does not exist, so the write fails.
    run(&mut ed, ":w no/such/dir/b.py<ret>");
    let err = status_error(&ed);
    assert!(err.contains("no/such/dir/b.py"), "names the target: {err}");
    assert_eq!(ed.doc().path(), Some(path.as_path()), "name unchanged");
    assert!(ed.doc().is_modified(), "still modified");
    assert_eq!(ed.buffer().syntax.language(), Some(Language::Rust));
    assert!(!dir.join("no").exists());

    // A plain :w still goes to the original file.
    run(&mut ed, ":w<ret>");
    assert!(!ed.doc().is_modified(), "status: {:?}", ed.status);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "Xfn a() {}\n");

    // A successful :w <path> renames the buffer and redetects the language.
    run(&mut ed, "iY<esc>:w b.py<ret>");
    let renamed = dir.join("b.py");
    assert_eq!(ed.doc().path(), Some(renamed.as_path()));
    assert!(!ed.doc().is_modified(), "status: {:?}", ed.status);
    assert_eq!(ed.buffer().syntax.language(), Some(Language::Python));
    assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "XYfn a() {}\n");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "Xfn a() {}\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_failed_write_of_an_unnamed_buffer_leaves_it_unnamed() {
    let dir = save_dir("unnamed");
    let mut ed = editor("");
    ed.cwd = dir.clone();
    run(&mut ed, "ihello<esc>:w missing/x.py<ret>");
    status_error(&ed);
    assert_eq!(ed.doc().path(), None);
    assert!(ed.doc().is_modified());
    assert_eq!(ed.buffer().syntax.language(), None);
    run(&mut ed, ":w<ret>");
    assert!(status_error(&ed).contains("no file name"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_failed_write_quit_to_a_new_path_neither_renames_nor_quits() {
    let dir = save_dir("wq");
    let path = dir.join("a.txt");
    std::fs::write(&path, "a\n").unwrap();
    let mut ed = editor("");
    ed.cwd = dir.clone();
    ed.open(&path).unwrap();
    run(&mut ed, "iZ<esc>:wq gone/b.txt<ret>");
    status_error(&ed);
    assert!(!ed.should_quit);
    assert_eq!(ed.doc().path(), Some(path.as_path()));
    assert!(ed.doc().is_modified());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn a_write_refused_by_permissions_keeps_the_old_name() {
    use std::os::unix::fs::PermissionsExt;
    let dir = save_dir("perm");
    let locked = dir.join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root ignores directory permissions; nothing to test then.
    let probe = locked.join("probe");
    if std::fs::write(&probe, "").is_ok() {
        let _ = std::fs::remove_file(&probe);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        return;
    }
    let path = dir.join("a.txt");
    std::fs::write(&path, "a\n").unwrap();
    let mut ed = editor("");
    ed.cwd = dir.clone();
    ed.open(&path).unwrap();
    run(&mut ed, "iP<esc>:w locked/b.txt<ret>");
    let err = status_error(&ed);
    assert!(err.contains("locked/b.txt"), "names the target: {err}");
    assert_eq!(ed.doc().path(), Some(path.as_path()));
    assert!(ed.doc().is_modified());
    run(&mut ed, ":w<ret>");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "Pa\n");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
