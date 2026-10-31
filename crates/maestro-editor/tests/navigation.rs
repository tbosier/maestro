//! Navigation behavior: the background file finder, workspace search, the
//! picker preview, session restore and recent files. Everything is driven
//! through `Editor::handle_key` and `Editor::poll_background`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use maestro_config::Config;
use maestro_editor::navigation::{self, session};
use maestro_editor::picker::PickerAction;
use maestro_editor::{Editor, KeyCode, KeyEvent, Rect, Severity};

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

/// A fresh, empty directory for one test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("maestro-nav-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(dir).unwrap()
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// An editor rooted at `cwd`, wide enough for the picker preview.
fn editor_in(cwd: &Path) -> Editor {
    let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 160, 40));
    ed.cwd = cwd.to_path_buf();
    ed
}

/// Applies background results until the picker stops loading.
fn wait_loaded(ed: &mut Editor) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        ed.poll_background();
        match &ed.picker {
            Some(p) if p.loading => {}
            _ => return,
        }
        assert!(Instant::now() < deadline, "picker still loading after 20 s");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Applies background results until no background job is running.
fn wait_idle(ed: &mut Editor) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while ed.has_background_work() {
        ed.poll_background();
        assert!(Instant::now() < deadline, "background work still running");
        std::thread::sleep(Duration::from_millis(2));
    }
    ed.poll_background();
}

fn labels(ed: &Editor) -> Vec<String> {
    let p = ed.picker.as_ref().expect("picker open");
    p.matches()
        .iter()
        .map(|m| p.items()[m.index].label.clone())
        .collect()
}

fn doc_path(ed: &Editor) -> PathBuf {
    ed.doc().path().expect("named buffer").to_path_buf()
}

#[test]
fn finder_respects_gitignore_and_hidden_files() {
    let dir = temp_dir("finder");
    write(&dir, ".gitignore", "ignored/\n*.log\n");
    write(&dir, "src/main.rs", "fn main() {}\n");
    write(&dir, "notes.txt", "notes\n");
    write(&dir, "ignored/skip.rs", "x\n");
    write(&dir, "debug.log", "x\n");
    write(&dir, ".hidden/secret.rs", "x\n");
    write(&dir, "sub/.ignore", "local.tmp\n");
    write(&dir, "sub/local.tmp", "x\n");
    write(&dir, "sub/kept.rs", "x\n");
    let mut ed = editor_in(&dir);
    run(&mut ed, "<space>f");
    assert!(ed.picker.as_ref().unwrap().loading);
    wait_loaded(&mut ed);
    let mut got = labels(&ed);
    got.sort();
    assert_eq!(got, vec!["notes.txt", "src/main.rs", "sub/kept.rs"]);

    // The preview shows the selected file from its first line.
    run(&mut ed, "main");
    let pv = ed
        .picker
        .as_ref()
        .unwrap()
        .preview
        .clone()
        .expect("preview");
    assert_eq!(pv.path, dir.join("src/main.rs"));
    assert_eq!(pv.lines[0], "fn main() {}");

    run(&mut ed, "\n");
    assert!(ed.picker.is_none());
    assert_eq!(doc_path(&ed), dir.join("src/main.rs"));
}

#[test]
fn results_for_a_closed_picker_are_dropped() {
    let dir = temp_dir("closed");
    for i in 0..50 {
        write(&dir, &format!("f{i}.txt"), "x\n");
    }
    let mut ed = editor_in(&dir);
    run(&mut ed, "<space>f<esc>");
    assert!(ed.picker.is_none());
    // A different picker opened before the walk's results are applied must
    // not receive them.
    run(&mut ed, "<space>b");
    let before = ed.picker.as_ref().unwrap().total();
    wait_idle(&mut ed);
    let p = ed.picker.as_ref().unwrap();
    assert_eq!(p.title, "buffers");
    assert_eq!(p.total(), before);
    assert!(
        p.items()
            .iter()
            .all(|i| matches!(i.action, PickerAction::Buffer(_)))
    );
}

#[test]
fn picker_stays_usable_while_results_stream_in() {
    let dir = temp_dir("usable");
    for d in 0..20 {
        for f in 0..100 {
            write(&dir, &format!("d{d}/file{f}.txt"), "");
        }
    }
    write(&dir, "d7/target_here.rs", "");
    let mut ed = editor_in(&dir);
    run(&mut ed, "<space>f");
    // Type a query before anything has arrived; it filters what streams in.
    run(&mut ed, "targethere");
    wait_loaded(&mut ed);
    assert_eq!(ed.picker.as_ref().unwrap().total(), 2001);
    assert_eq!(labels(&ed), vec!["d7/target_here.rs"]);
}

#[test]
fn workspace_search_finds_matches_and_opens_at_line_and_column() {
    let dir = temp_dir("grep");
    write(&dir, ".gitignore", "skipped.txt\n");
    write(&dir, "a.rs", "fn main() {}\nlet αβγ = needle;\n");
    write(&dir, "b.txt", "nothing\n\tééNeedle here\nlast\n");
    write(&dir, "skipped.txt", "needle\n");
    write(&dir, "bin.dat", "needle\0\n");
    let mut ed = editor_in(&dir);
    run(&mut ed, "<space>/needle\n");
    assert!(ed.prompt.is_none());
    wait_loaded(&mut ed);
    let mut got = labels(&ed);
    got.sort();
    assert_eq!(
        got,
        vec!["a.rs:2:11  let αβγ = needle;", "b.txt:2:4  ééNeedle here"]
    );

    // Narrow to the b.txt result; the preview highlights its line.
    run(&mut ed, "b.txt");
    let pv = ed
        .picker
        .as_ref()
        .unwrap()
        .preview
        .clone()
        .expect("preview");
    assert_eq!(pv.path, dir.join("b.txt"));
    assert_eq!(pv.target, Some(1));
    assert_eq!(pv.lines[1 - pv.first_line], "\tééNeedle here");

    run(&mut ed, "\n");
    assert_eq!(doc_path(&ed), dir.join("b.txt"));
    let text = ed.doc().text().clone();
    let head = ed.view().selection.primary().head;
    assert_eq!(text.char_to_line(head), 1);
    assert_eq!(head - text.line_to_char(1), 3);
    assert_eq!(text.char(head), 'N');

    // The other result, through :grep, with multi-byte chars before it.
    run(&mut ed, ":grep needle\n");
    wait_loaded(&mut ed);
    assert_eq!(ed.picker.as_ref().unwrap().title, "search /needle/");
    run(&mut ed, "a.rs\n");
    assert_eq!(doc_path(&ed), dir.join("a.rs"));
    let text = ed.doc().text().clone();
    let head = ed.view().selection.primary().head;
    assert_eq!(text.char_to_line(head), 1);
    assert_eq!(text.slice(head..head + 6).to_string(), "needle");
}

#[test]
fn workspace_search_uses_smart_case_and_reports_bad_patterns() {
    let dir = temp_dir("grepcase");
    write(&dir, "a.txt", "Foo\nfoo\n");
    let mut ed = editor_in(&dir);
    run(&mut ed, ":grep Foo\n");
    wait_loaded(&mut ed);
    assert_eq!(labels(&ed), vec!["a.txt:1:1  Foo"]);
    run(&mut ed, "<esc><space>/(\n");
    assert!(ed.picker.is_none());
    let (sev, msg) = ed.status.clone().expect("error shown");
    assert_eq!(sev, Severity::Error);
    assert!(msg.contains("invalid regex"), "{msg}");
}

#[test]
fn workspace_search_caps_results_and_says_so() {
    let dir = temp_dir("grepcap");
    let text: String = (0..10_050).map(|i| format!("hit {i}\n")).collect();
    write(&dir, "many.txt", &text);
    let mut ed = editor_in(&dir);
    run(&mut ed, ":grep hit\n");
    wait_loaded(&mut ed);
    let p = ed.picker.as_ref().unwrap();
    assert_eq!(p.total(), navigation::grep::RESULT_LIMIT);
    assert_eq!(p.title, "search /hit/ (first 10000 matches)");
}

fn configured(cwd: &Path, state: &Path) -> Editor {
    let mut ed = editor_in(cwd);
    navigation::configure(&mut ed, Some(state.to_path_buf()), true);
    ed
}

#[test]
fn session_is_saved_on_quit_and_restored() {
    let dir = temp_dir("session");
    let state = dir.join("state");
    write(&dir, "one.txt", "first\nsecond line\nthird\n");
    write(&dir, "two.txt", "αβγ δεζ\nmore\n");
    write(&dir, "gone.txt", "temporary\n");

    let mut ed = configured(&dir, &state);
    run(&mut ed, ":e gone.txt\n:e one.txt\n:2\nww");
    let one_head = ed.view().selection.primary().head;
    run(&mut ed, ":e two.txt\nw");
    let two_head = ed.view().selection.primary().head;
    assert!(two_head > 0);
    run(&mut ed, ":e one.txt\n:qa\n");
    assert!(ed.should_quit);
    let file = session::session_path(&state, &dir);
    assert!(file.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    let mut ed = configured(&dir, &state);
    session::restore(&mut ed);
    let mut paths: Vec<PathBuf> = ed
        .buffers
        .values()
        .filter_map(|b| b.doc.path().map(Path::to_path_buf))
        .collect();
    paths.sort();
    assert_eq!(paths, vec![dir.join("one.txt"), dir.join("two.txt")]);
    assert_eq!(ed.buffers.len(), 2, "the startup scratch buffer is gone");
    // The focused view shows the file that was focused, at its cursor.
    assert_eq!(doc_path(&ed), dir.join("one.txt"));
    assert_eq!(ed.view().selection.primary().head, one_head);
    let (sev, msg) = ed.status.clone().unwrap();
    assert_eq!(sev, Severity::Warning);
    assert!(msg.contains("restored session: 2 files"), "{msg}");
    assert!(msg.contains("skipped missing: gone.txt"), "{msg}");
    // The other buffer's cursor comes back when it is shown.
    run(&mut ed, ":e two.txt\n");
    assert_eq!(ed.view().selection.primary().head, two_head);
}

#[test]
fn splits_are_saved_and_restored() {
    let dir = temp_dir("session-splits");
    let state = dir.join("state");
    let long: String = (0..200).map(|i| format!("line {i}\n")).collect();
    write(&dir, "one.txt", &long);
    write(&dir, "two.txt", "alpha\nbeta\ngamma\n");
    write(&dir, "gone.txt", "temporary\n");

    // Left: one.txt scrolled down. Right: two.txt over gone.txt.
    let mut ed = configured(&dir, &state);
    run(&mut ed, ":e one.txt\n150G");
    run(&mut ed, ":vsplit two.txt\njw");
    run(&mut ed, ":split gone.txt\n");
    run(&mut ed, "<C-w>k");
    assert_eq!(doc_path(&ed), dir.join("two.txt"));
    let left_top = ed.views[&ed.layout.views()[0]].top_line;
    assert!(
        left_top > 100,
        "left view should be scrolled, top {left_top}"
    );
    let two_head = ed.view().selection.primary().head;
    run(&mut ed, ":qa\n");
    assert!(ed.should_quit);

    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    let mut ed = configured(&dir, &state);
    session::restore(&mut ed);
    // gone.txt's view is dropped and its split collapses: two side by side.
    let order = ed.layout.views();
    assert_eq!(order.len(), 2);
    assert_eq!(ed.views.len(), 2, "no orphaned views");
    let path_of = |ed: &Editor, i: usize| {
        let v = &ed.views[&ed.layout.views()[i]];
        ed.buffers[&v.doc].doc.path().unwrap().to_path_buf()
    };
    assert_eq!(path_of(&ed, 0), dir.join("one.txt"));
    assert_eq!(path_of(&ed, 1), dir.join("two.txt"));
    let rects: Vec<_> = order.iter().map(|v| ed.views[v].area).collect();
    assert_eq!(rects[0].y, rects[1].y, "side by side");
    assert!(rects[0].x < rects[1].x);
    // Each view keeps its own cursor and scroll; focus is restored.
    assert_eq!(ed.focus, order[1]);
    assert_eq!(ed.view().selection.primary().head, two_head);
    assert_eq!(ed.views[&order[0]].top_line, left_top);
    let left = &ed.views[&order[0]];
    let text = ed.buffers[&left.doc].doc.text();
    assert_eq!(text.char_to_line(left.selection.primary().head), 149);
    assert_eq!(ed.buffers.len(), 2, "the startup scratch buffer is gone");
}

#[test]
fn sessions_are_per_directory_and_can_be_disabled() {
    let dir = temp_dir("session-off");
    let state = dir.join("state");
    write(&dir, "a.txt", "a\n");
    let other = dir.join("other");
    std::fs::create_dir_all(&other).unwrap();

    let mut ed = editor_in(&dir);
    navigation::configure(&mut ed, Some(state.clone()), false);
    run(&mut ed, ":e a.txt\n:qa\n");
    assert!(!session::session_path(&state, &dir).exists());

    let mut ed = configured(&dir, &state);
    run(&mut ed, ":e a.txt\n:qa\n");
    let mut ed = configured(&other, &state);
    session::restore(&mut ed);
    assert!(ed.doc().path().is_none(), "nothing restored in another cwd");
}

#[test]
fn recent_files_are_most_recent_first_across_sessions() {
    let dir = temp_dir("recent");
    let state = dir.join("state");
    for f in ["a.txt", "b.txt", "c.txt"] {
        write(&dir, f, "x\n");
    }
    let mut ed = configured(&dir, &state);
    run(&mut ed, ":e a.txt\n:e b.txt\n:e c.txt\n:e a.txt\n<space>r");
    assert_eq!(labels(&ed), vec!["a.txt", "c.txt", "b.txt"]);
    run(&mut ed, "c\n");
    assert_eq!(doc_path(&ed), dir.join("c.txt"));

    // A new editor sees the same list; deleted files are left out.
    std::fs::remove_file(dir.join("b.txt")).unwrap();
    let mut ed = configured(&dir, &state);
    run(&mut ed, "<space>r");
    assert_eq!(labels(&ed), vec!["c.txt", "a.txt"]);
}

#[test]
fn a_result_from_disk_never_lands_on_another_line_of_an_edited_buffer() {
    let dir = temp_dir("goto-edited");
    write(&dir, "a.txt", "prefix needle\nsecond\n");
    let mut ed = editor_in(&dir);
    // Open the file and shorten the first line without saving.
    run(&mut ed, ":e a.txt\nxcx\n<esc>");
    assert_eq!(ed.doc().text().to_string(), "x\nsecond\n");
    run(&mut ed, ":grep needle\n");
    wait_loaded(&mut ed);
    run(&mut ed, "\n");
    let text = ed.doc().text().clone();
    let head = ed.view().selection.primary().head;
    assert_eq!(text.char_to_line(head), 0, "cursor stayed on line 1");
}

#[test]
fn grep_keeps_the_spaces_in_its_pattern() {
    let dir = temp_dir("grep-spaces");
    write(&dir, "a.txt", "a b\na  b\n");
    let mut ed = editor_in(&dir);
    run(&mut ed, ":grep a  b\n");
    wait_loaded(&mut ed);
    assert_eq!(ed.picker.as_ref().unwrap().title, "search /a  b/");
    assert_eq!(labels(&ed), vec!["a.txt:2:1  a  b"]);
}

#[test]
fn restoring_a_session_does_not_reorder_recent_files() {
    let dir = temp_dir("restore-recent");
    let state = dir.join("state");
    write(&dir, "a.txt", "a\n");
    write(&dir, "b.txt", "b\n");
    let mut ed = configured(&dir, &state);
    run(&mut ed, ":e a.txt\n:e b.txt\n:e a.txt\n:qa\n");
    let mut ed = configured(&dir, &state);
    session::restore(&mut ed);
    assert_eq!(doc_path(&ed), dir.join("a.txt"));
    run(&mut ed, "<space>r");
    assert_eq!(labels(&ed), vec!["a.txt", "b.txt"]);
}

/// Measures the finder on a generated tree (200,000 files by default, or
/// `MAESTRO_NAV_BENCH_FILES`). Run with
/// `cargo test --release -p maestro-editor --test navigation -- --ignored --nocapture`.
///
/// It imitates the terminal loop: apply background results, then handle a
/// key, then sleep for about one frame (nothing is rendered). It reports the
/// slowest poll that applied results,
/// the slowest key, and the longest run of back-to-back result polls, which is
/// what would keep the real loop from reading input.
#[test]
#[ignore]
fn large_tree_streams_without_blocking_input() {
    let n: usize = std::env::var("MAESTRO_NAV_BENCH_FILES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    let root = std::env::temp_dir().join(format!("maestro-nav-bench-{n}"));
    let marker = root.join(".complete");
    if !marker.exists() {
        let _ = std::fs::remove_dir_all(&root);
        for i in 0..n {
            let dir = root.join(format!("d{:03}/s{:02}", i % 500, (i / 500) % 40));
            if i < 20_000 {
                std::fs::create_dir_all(&dir).unwrap();
            }
            std::fs::write(dir.join(format!("file_{i}.rs")), "").unwrap();
        }
        std::fs::write(&marker, "").unwrap();
    }
    let mut ed = editor_in(&root);
    let start = Instant::now();
    run(&mut ed, "<space>f");
    let open_ms = start.elapsed().as_secs_f64() * 1e3;
    let mut first_items = None;
    let (mut max_poll, mut max_key, mut max_gap) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let (mut streak, mut max_streak, mut batches) = (0, 0, 0);
    let typed = ['f', 'i', 'l', 'e', '9', '9'];
    let mut k = 0;
    let mut last_input = Instant::now();
    let mut key_times = Vec::new();
    let frame = Duration::from_millis(1);
    while ed.picker.as_ref().unwrap().loading {
        // Like the terminal loop: queued results first, then input.
        let t = Instant::now();
        if ed.poll_background() {
            max_poll = max_poll.max(t.elapsed());
            batches += 1;
            streak += 1;
            max_streak = max_streak.max(streak);
            if first_items.is_none() {
                first_items = Some(start.elapsed());
            }
            std::thread::sleep(frame);
            continue;
        }
        streak = 0;
        // Type a query and erase it again, one key per loop turn.
        let key = if k < typed.len() {
            KeyEvent::char(typed[k])
        } else {
            KeyEvent::plain(KeyCode::Backspace)
        };
        k = (k + 1) % (2 * typed.len());
        max_gap = max_gap.max(last_input.elapsed());
        let t = Instant::now();
        ed.handle_key(key);
        max_key = max_key.max(t.elapsed());
        key_times.push(t.elapsed());
        last_input = Instant::now();
        std::thread::sleep(frame);
        assert!(start.elapsed() < Duration::from_secs(120));
    }
    let total = start.elapsed();
    assert_eq!(ed.picker.as_ref().unwrap().total(), n);
    key_times.sort();
    let pct = |p: usize| {
        key_times
            .get((key_times.len() * p / 100).min(key_times.len().saturating_sub(1)))
            .copied()
            .unwrap_or_default()
            .as_secs_f64()
            * 1e3
    };
    // Filtering the full list from an empty query, one key at a time.
    run(&mut ed, "<C-u>");
    let mut filter_max = Duration::ZERO;
    for c in "file_199999".chars() {
        let t = Instant::now();
        ed.handle_key(KeyEvent::char(c));
        filter_max = filter_max.max(t.elapsed());
    }
    assert_eq!(labels(&ed)[0], "d499/s39/file_199999.rs");
    eprintln!(
        "files={n} open_ms={open_ms:.2} first_results_applied_ms={:.1} all_ms={:.0} polls_applying_results={batches} \
         max_result_poll_ms={:.2} keys={} key_p50_ms={:.2} key_p95_ms={:.2} max_key_while_loading_ms={:.2} max_ms_between_keys={:.2} \
         max_back_to_back_result_polls={max_streak} max_key_filtering_full_list_ms={:.2}",
        first_items.unwrap_or_default().as_secs_f64() * 1e3,
        total.as_secs_f64() * 1e3,
        max_poll.as_secs_f64() * 1e3,
        key_times.len(),
        pct(50),
        pct(95),
        max_key.as_secs_f64() * 1e3,
        max_gap.as_secs_f64() * 1e3,
        filter_max.as_secs_f64() * 1e3,
    );
    assert!(max_streak <= 1, "batches arrived back to back");
}
