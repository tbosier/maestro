//! The message log and `:messages`: every status message is kept, so one
//! the status line cut off or a later one replaced can be read again.

use maestro_config::Config;
use maestro_core::Document;
use maestro_editor::messages::LOG_LEN;
use maestro_editor::{Editor, KeyCode, KeyEvent, Rect, Severity};

/// Parses a compact key script: plain characters are typed as-is, `<...>`
/// holds key notation and a newline is Enter.
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

fn editor() -> Editor {
    let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 80, 24));
    let id = ed.add_document(Document::from_text("text\n"));
    ed.switch_to(id);
    ed
}

fn run(ed: &mut Editor, script: &str) {
    for k in keys(script) {
        ed.handle_key(k);
    }
}

fn logged(ed: &Editor) -> Vec<(Severity, String)> {
    ed.messages
        .iter()
        .map(|m| (m.severity, m.text.clone()))
        .collect()
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("maestro-msg-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

#[test]
fn messages_accumulate_in_order_with_their_severity() {
    let dir = temp_dir("order");
    let mut ed = editor();
    run(&mut ed, ":no-such-one\n");
    run(&mut ed, &format!(":cd {}\n", dir.display()));
    run(&mut ed, ":no-such-two\n");
    assert_eq!(
        logged(&ed),
        [
            (Severity::Error, "unknown command: no-such-one".to_string()),
            (Severity::Info, format!("cwd: {}", dir.display())),
            (Severity::Error, "unknown command: no-such-two".to_string()),
        ]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_log_keeps_only_the_newest_messages() {
    let mut ed = editor();
    for i in 0..LOG_LEN + 3 {
        run(&mut ed, &format!(":nope{i}\n"));
    }
    assert_eq!(ed.messages.len(), LOG_LEN);
    let first = ed.messages.iter().next().unwrap();
    assert_eq!(first.text, "unknown command: nope3");
    let last = ed.messages.iter().next_back().unwrap();
    assert_eq!(last.text, format!("unknown command: nope{}", LOG_LEN + 2));
}

#[test]
fn messages_shows_what_the_status_line_replaced() {
    let mut ed = editor();
    run(&mut ed, ":first-gone\n");
    run(&mut ed, ":second-shown\n");
    // The status line holds only the newer message.
    let (_, shown) = ed.status.clone().unwrap();
    assert_eq!(shown, "unknown command: second-shown");

    run(&mut ed, ":messages\n");
    let text = ed.doc().text().to_string();
    let lines: Vec<&str> = text.lines().collect();
    let first = lines
        .iter()
        .position(|l| l.ends_with("error    unknown command: first-gone"))
        .unwrap_or_else(|| panic!("{text}"));
    let second = lines
        .iter()
        .position(|l| l.ends_with("error    unknown command: second-shown"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(first < second, "newest last: {text}");
    // The cursor is on the newest message.
    let head = ed.view().selection.primary().head;
    let line = ed.doc().text().char_to_line(head);
    assert_eq!(line, second);
    // Nothing to save, so quitting is not blocked by it.
    run(&mut ed, ":q\n");
    assert!(ed.should_quit, "{:?}", ed.status);
}

#[test]
fn messages_again_replaces_its_earlier_buffer() {
    let mut ed = editor();
    run(&mut ed, ":one\n:messages\n");
    let before = ed.buffers.len();
    run(&mut ed, "ga");
    run(&mut ed, ":two\n:messages\n");
    assert_eq!(ed.buffers.len(), before);
    assert!(ed.doc().text().to_string().contains("unknown command: two"));
}

#[test]
fn the_command_palette_lists_and_runs_messages() {
    let mut ed = editor();
    run(&mut ed, ":no-such-before\n");
    run(&mut ed, "<space>?");
    let picker = ed.picker.as_ref().expect("palette open");
    assert!(
        picker.items().iter().any(|i| i
            .label
            .starts_with(":messages  Show earlier status messages")),
        "not listed"
    );
    run(&mut ed, ":messages\n");
    assert!(ed.picker.is_none());
    assert!(
        ed.doc()
            .text()
            .to_string()
            .contains("unknown command: no-such-before")
    );
}

#[test]
fn an_empty_log_says_so() {
    let mut ed = editor();
    run(&mut ed, ":messages\n");
    assert!(ed.doc().text().to_string().contains("No messages yet."));
}

#[test]
fn paths_inside_the_working_directory_print_relative() {
    let mut ed = editor();
    ed.cwd = std::path::PathBuf::from("/work/proj");
    assert_eq!(
        ed.show_path(std::path::Path::new("/work/proj/src/lib.rs")),
        "src/lib.rs"
    );
    // Outside the working directory (and the home directory) the path
    // stays absolute.
    let outside = std::path::Path::new("/elsewhere-maestro-test/src/lib.rs");
    assert_eq!(ed.show_path(outside), outside.display().to_string());
}

#[test]
fn a_reload_message_names_the_file_relative_to_the_working_directory() {
    let dir = temp_dir("reload");
    let other = temp_dir("reload-other");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let inside = dir.join("src/lib.rs");
    let outside = other.join("lib.rs");
    std::fs::write(&inside, "v1\n").unwrap();
    std::fs::write(&outside, "v1\n").unwrap();
    let mut ed = editor();
    run(&mut ed, &format!(":cd {}\n", dir.display()));
    ed.open(&inside).unwrap();
    std::fs::write(&inside, "version two\n").unwrap();
    ed.tick(std::time::Instant::now());
    let (_, msg) = ed.status.clone().unwrap();
    assert_eq!(msg, "reloaded src/lib.rs: it changed on disk");

    ed.open(&outside).unwrap();
    std::fs::write(&outside, "version two\n").unwrap();
    ed.tick(std::time::Instant::now());
    let (_, msg) = ed.status.clone().unwrap();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let expected = match home.as_deref().and_then(|h| outside.strip_prefix(h).ok()) {
        Some(rel) => format!("~/{}", rel.display()),
        None => outside.display().to_string(),
    };
    assert_eq!(msg, format!("reloaded {expected}: it changed on disk"));
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&other).unwrap();
}

#[test]
fn a_long_message_is_whole_in_messages_and_wrapped_to_the_view() {
    let mut ed = editor();
    let name = "q".repeat(150);
    run(&mut ed, &format!(":{name}\n:messages\n"));
    let text = ed.doc().text().to_string();
    let lines: Vec<&str> = text.lines().skip(2).collect();
    assert!(lines.len() > 1, "{text}");
    // No line is wider than the 80-column view less its gutter.
    assert!(lines.iter().all(|l| l.chars().count() <= 72), "{text}");
    // The first line breaks at the space; the long word breaks inside.
    assert!(lines[0].ends_with("error    unknown command:"), "{text}");
    // Joined back, the wrapped lines hold the whole message.
    let joined: String = lines.iter().map(|l| l[19..].trim_start()).collect();
    assert_eq!(joined, format!("unknown command:{name}"));
}
