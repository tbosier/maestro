//! `toggle_comment` (C-c): key sequences in, text and selections out.

use maestro_config::Config;
use maestro_core::Document;
use maestro_editor::{Editor, KeyCode, KeyEvent, Rect, Severity};

/// Plain characters are typed as-is; `<...>` holds key notation.
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

fn text(ed: &Editor) -> String {
    ed.doc().text().to_string()
}

/// Opens `text` as a file named `name` in a fresh directory, so the
/// language comes from the file name.
fn on_file(name: &str, text: &str) -> (Editor, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "maestro-comment-{}-{}",
        name.replace('.', "-"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 80, 24));
    let id = ed.add_document(Document::from_text(""));
    ed.switch_to(id);
    ed.cwd = dir.clone();
    ed.open(&path).unwrap();
    (ed, dir)
}

#[test]
fn c_c_comments_the_cursor_line_and_again_uncomments_it() {
    let (mut ed, dir) = on_file("a.py", "def f():\n    return 1\n");
    run(&mut ed, "jgs<C-c>");
    assert_eq!(text(&ed), "def f():\n    # return 1\n");
    // The cursor stays on the character it was on.
    let head = ed.view().selection.primary().head;
    assert_eq!(ed.doc().text().char(head), 'r');
    run(&mut ed, "<C-c>");
    assert_eq!(text(&ed), "def f():\n    return 1\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_selection_comments_its_lines_at_the_smallest_indent_and_skips_blanks() {
    let src = "if a:\n    b = 1\n\n    if c:\n        d()\n";
    let (mut ed, dir) = on_file("b.py", src);
    run(&mut ed, "jxxxx<C-c>");
    assert_eq!(
        text(&ed),
        "if a:\n    # b = 1\n\n    # if c:\n    #     d()\n"
    );
    // The selection still covers whole lines, so a second C-c undoes it.
    run(&mut ed, "<C-c>");
    assert_eq!(text(&ed), src);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_partly_commented_block_is_commented_not_uncommented() {
    let (mut ed, dir) = on_file("c.py", "# a = 1\nb = 2\n");
    run(&mut ed, "xx<C-c>");
    assert_eq!(text(&ed), "# # a = 1\n# b = 2\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn uncommenting_removes_the_token_and_one_space() {
    let (mut ed, dir) = on_file("d.py", "#x\n#  y\n");
    run(&mut ed, "xx<C-c>");
    assert_eq!(text(&ed), "x\n y\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn every_cursor_toggles_its_own_lines_in_one_undo_step() {
    let (mut ed, dir) = on_file("e.rs", "fn a() {}\n// fn b() {}\nfn c() {}\n");
    // Three cursors, one per line.
    run(&mut ed, "CC<C-c>");
    assert_eq!(text(&ed), "// fn a() {}\nfn b() {}\n// fn c() {}\n");
    run(&mut ed, "u");
    assert_eq!(text(&ed), "fn a() {}\n// fn b() {}\nfn c() {}\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn two_cursors_on_one_line_toggle_it_once() {
    let (mut ed, dir) = on_file("f.py", "a = b\n");
    run(&mut ed, "%sa|b<ret><C-c>");
    assert_eq!(ed.view().selection.len(), 2);
    assert_eq!(text(&ed), "# a = b\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn dot_repeats_the_toggle() {
    let (mut ed, dir) = on_file("g.toml", "a = 1\nb = 2\n");
    run(&mut ed, "<C-c>j.");
    assert_eq!(text(&ed), "# a = 1\n# b = 2\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ctrl_slash_works_as_either_key_terminals_send() {
    for key in ["<C-/>", "<C-7>"] {
        let (mut ed, dir) = on_file("h.ts", "let a = 1;\n");
        run(&mut ed, key);
        assert_eq!(text(&ed), "// let a = 1;\n", "{key}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn select_mode_toggles_too() {
    let (mut ed, dir) = on_file("i.sh", "echo a\necho b\n");
    run(&mut ed, "vj<C-c>");
    assert_eq!(text(&ed), "# echo a\n# echo b\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn files_without_line_comments_change_nothing_and_say_so() {
    for name in ["j.json", "notes.txt"] {
        let (mut ed, dir) = on_file(name, "{}\n");
        run(&mut ed, "<C-c>");
        assert_eq!(text(&ed), "{}\n", "{name}");
        assert!(!ed.doc().is_modified());
        let (severity, msg) = ed.status.clone().expect("a status");
        assert_eq!(severity, Severity::Warning);
        assert!(msg.starts_with("toggle comment:"), "{msg}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    // A buffer with no file at all.
    let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 80, 24));
    let id = ed.add_document(Document::from_text("x\n"));
    ed.switch_to(id);
    run(&mut ed, "<C-c>");
    assert_eq!(text(&ed), "x\n");
    assert!(ed.status.is_some());
}

#[test]
fn a_blank_line_alone_is_left_alone() {
    let (mut ed, dir) = on_file("k.py", "\n");
    run(&mut ed, "<C-c>");
    assert_eq!(text(&ed), "\n");
    assert!(!ed.doc().is_modified());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_typed_command_toggles_too() {
    let (mut ed, dir) = on_file("l.py", "x = 1\n");
    run(&mut ed, ":toggle-comment<ret>");
    assert_eq!(text(&ed), "# x = 1\n");
    std::fs::remove_dir_all(&dir).unwrap();
}
