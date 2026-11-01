//! Structural pipelines (`:select-nodes`, filters, `:expand-to`,
//! `:insert-above`), driven through keys.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use maestro_config::Config;
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

/// Types a `:` command line and Enter.
fn command(ed: &mut Editor, line: &str) {
    run(ed, ":");
    for c in line.chars() {
        ed.handle_key(KeyEvent::char(c));
    }
    ed.handle_key(KeyEvent::plain(KeyCode::Enter));
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("maestro-pipelines-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

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
    let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 120, 40));
    ed.open(path).unwrap();
    settle(&mut ed);
    ed
}

fn text(ed: &Editor) -> String {
    ed.doc().text().to_string()
}

fn selections(ed: &Editor) -> Vec<String> {
    let t = ed.doc().text().slice(..);
    ed.view().selection.iter().map(|r| r.fragment(t)).collect()
}

fn status(ed: &Editor) -> (Severity, String) {
    ed.status.clone().expect("a status message")
}

/// The function name in each selection, for compact assertions.
fn fn_names(ed: &Editor) -> Vec<String> {
    selections(ed)
        .iter()
        .map(|s| {
            let after = &s[s.find("fn ").expect("a function") + 3..];
            after
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap()
                .to_string()
        })
        .collect()
}

/// A mix of functions for the spec's example: "Select every public async
/// function that returns Result and has no tracing instrumentation."
const FIXTURE: &str = r#"//! Fixture for the structural pipeline test.

use std::io;

/// A doc example that must not match:
///
/// ```
/// pub async fn in_doc_comment() -> Result<(), Error> { Ok(()) }
/// ```
pub async fn fetch(url: &str) -> Result<String, Error> {
    let _s = "pub async fn in_string() -> Result<(), Error> { Ok(()) }";
    let _ = url;
    todo!()
}

#[tracing::instrument]
pub async fn already_traced() -> Result<(), Error> {
    Ok(())
}

#[instrument(skip(db))]
#[allow(clippy::too_many_arguments)]
pub async fn also_traced(db: u8) -> Result<(), Error> {
    let _ = db;
    Ok(())
}

pub async fn no_result() -> u32 {
    fn helper() -> Result<(), Error> {
        Ok(())
    }
    1
}

pub fn sync_one() -> Result<(), Error> {
    Ok(())
}

async fn private_one() -> Result<(), Error> {
    Ok(())
}

pub(crate) async fn crate_one() -> Result<(), Error> {
    Ok(())
}

pub async fn io_result() -> io::Result<()> {
    Ok(())
}

#[instrument]
fn traced_private() {}

pub async fn after_traced_private() -> std::result::Result<u8, Error> {
    Ok(1)
}

pub mod nested {
    pub mod deeper {
        /// Docs on a nested one.
        #[must_use]
        pub async fn deep() -> Result<u8, super::super::Error> {
            Ok(1)
        }
    }
}

pub struct Service;

impl Service {
    pub async fn method(&self) -> Result<(), Error> {
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn traced_method(&self) -> Result<(), Error> {
        Ok(())
    }

    async fn private_method(&self) -> Result<(), Error> {
        Ok(())
    }
}

pub trait Store {
    async fn get(&self) -> Result<u8, Error>;
}

macro_rules! make {
    () => {
        pub async fn in_macro() -> Result<(), Error> {
            Ok(())
        }
    };
}

pub struct Error;
"#;

const SPEC_PIPELINE: &str = "select-nodes pub-async-fn | keep-nodes returns-result | remove-nodes instrumented | insert-above #[tracing::instrument]";

/// `FIXTURE` with the attribute added by hand above the five functions the
/// spec's sentence describes.
fn expected_after_spec_pipeline() -> String {
    let mut out = FIXTURE.to_string();
    for (line, indent) in [
        ("pub async fn fetch(", ""),
        ("pub async fn io_result(", ""),
        ("pub async fn after_traced_private(", ""),
        ("pub async fn deep(", "        "),
        ("pub async fn method(", "    "),
    ] {
        let at = out.find(line).expect("fixture line") - indent.len();
        out.insert_str(at, &format!("{indent}#[tracing::instrument]\n"));
    }
    out
}

#[test]
fn spec_example_selects_exactly_the_right_functions_and_adds_the_attribute_as_one_undo_step() {
    let dir = temp_dir("spec");
    let mut ed = open(&dir.join("service.rs"), FIXTURE);

    // The selection steps alone first, to see what they select.
    command(
        &mut ed,
        "select-nodes pub-async-fn | keep-nodes returns-result | remove-nodes instrumented",
    );
    assert_eq!(
        status(&ed),
        (
            Severity::Info,
            "select-nodes 9 | keep-nodes 8 | remove-nodes 5".into()
        )
    );
    assert_eq!(
        fn_names(&ed),
        [
            "fetch",
            "io_result",
            "after_traced_private",
            "deep",
            "method"
        ]
    );
    assert!(selections(&ed)[0].starts_with("pub async fn fetch(url: &str)"));
    assert!(selections(&ed)[0].ends_with("todo!()\n}"));
    assert_eq!(text(&ed), FIXTURE, "selection steps change no text");

    // The whole pipeline, transformation included.
    run(&mut ed, "<esc>");
    command(&mut ed, SPEC_PIPELINE);
    assert_eq!(
        status(&ed),
        (
            Severity::Info,
            "select-nodes 9 | keep-nodes 8 | remove-nodes 5 | insert-above 5: inserted 5 lines (u undoes)"
                .into()
        )
    );
    assert_eq!(text(&ed), expected_after_spec_pipeline());
    // The selections still cover the functions, not the new lines.
    assert_eq!(
        fn_names(&ed),
        [
            "fetch",
            "io_result",
            "after_traced_private",
            "deep",
            "method"
        ]
    );
    assert!(
        selections(&ed)
            .iter()
            .all(|s| s.starts_with("pub async fn"))
    );

    // Running the selection steps again now finds nothing left to do.
    command(
        &mut ed,
        "select-nodes pub-async-fn | keep-nodes returns-result | remove-nodes instrumented",
    );
    let (sev, msg) = status(&ed);
    assert_eq!(sev, Severity::Error);
    assert_eq!(
        msg,
        "step 3 (remove-nodes) left no selections (after select-nodes 9 | keep-nodes 8); nothing changed"
    );

    // One undo step reverts all five insertions; redo brings them back.
    run(&mut ed, "u");
    assert_eq!(text(&ed), FIXTURE);
    run(&mut ed, "U");
    assert_eq!(text(&ed), expected_after_spec_pipeline());
    run(&mut ed, "u");
    assert_eq!(text(&ed), FIXTURE);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn raw_queries_give_the_same_result_as_the_shortcuts() {
    let dir = temp_dir("raw");
    let mut ed = open(&dir.join("service.rs"), FIXTURE);
    command(
        &mut ed,
        r#"select-nodes (function_item (visibility_modifier) @v (#eq? @v "pub") (function_modifiers "async")) @select | keep-nodes (function_item return_type: [(generic_type type: (type_identifier) @t) (generic_type type: (scoped_type_identifier name: (type_identifier) @t))] (#eq? @t "Result")) @select | remove-nodes ((attribute_item (attribute [(identifier) (scoped_identifier)] @a (#match? @a "^(tracing::)?instrument$"))) . [(attribute_item) (line_comment) (block_comment)]* . (function_item) @select) | insert-above #[tracing::instrument]"#,
    );
    let (sev, msg) = status(&ed);
    assert_eq!(sev, Severity::Info, "{msg}");
    assert_eq!(text(&ed), expected_after_spec_pipeline());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_queries_report_the_position_and_change_nothing() {
    let dir = temp_dir("invalid");
    let mut ed = open(&dir.join("service.rs"), FIXTURE);
    let before = ed.view().selection.clone();

    command(&mut ed, "select-nodes (function_item (nope))");
    assert_eq!(
        status(&ed),
        (
            Severity::Error,
            "select-nodes: invalid query at column 17: unknown node kind \"nope\"".into()
        )
    );
    command(&mut ed, "select-nodes (function_item");
    let (sev, msg) = status(&ed);
    assert_eq!(sev, Severity::Error);
    assert!(
        msg.starts_with("select-nodes: invalid query at column"),
        "{msg}"
    );
    assert!(msg.contains("syntax error"), "{msg}");

    // A bad third step stops the pipeline before the first one runs.
    command(
        &mut ed,
        "select-nodes fn | keep-nodes returns-result | remove-nodes (attribute_item (bogus)) | insert-above x",
    );
    assert_eq!(
        status(&ed),
        (
            Severity::Error,
            "step 3 (remove-nodes): invalid query at column 18: unknown node kind \"bogus\"".into()
        )
    );
    command(&mut ed, "select-nodes nonsense");
    let (_, msg) = status(&ed);
    assert!(
        msg.starts_with(
            "select-nodes: no rust shortcut named \"nonsense\" (shortcuts: fn, pub-fn,"
        ),
        "{msg}"
    );
    command(&mut ed, "expand-to fn");
    let (_, msg) = status(&ed);
    assert!(msg.contains("rust has no node kind \"fn\""), "{msg}");
    command(&mut ed, "keep-matching (unclosed");
    let (_, msg) = status(&ed);
    assert!(msg.starts_with("keep-matching: invalid regex"), "{msg}");
    command(
        &mut ed,
        "select-nodes fn | insert-above x | keep-matching y",
    );
    assert_eq!(
        status(&ed).1,
        "step 2 (insert-above): insert-above changes the text, so it must be the last step"
    );
    command(&mut ed, "select-nodes fn | | keep-matching y");
    assert_eq!(status(&ed).1, "step 2 is empty");
    command(&mut ed, "select-nodes fn | write");
    assert!(
        status(&ed)
            .1
            .starts_with("step 2 (write): write cannot be a pipeline step"),
        "{:?}",
        status(&ed)
    );

    assert_eq!(text(&ed), FIXTURE);
    assert_eq!(ed.view().selection, before);
    assert!(!ed.doc().is_modified());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_filter_that_empties_the_selection_says_which_step_and_changes_nothing() {
    let dir = temp_dir("empty");
    let mut ed = open(&dir.join("service.rs"), FIXTURE);
    let before = ed.view().selection.clone();
    command(
        &mut ed,
        "select-nodes pub-async-fn | keep-matching no_such_text | insert-above #[x]",
    );
    assert_eq!(
        status(&ed),
        (
            Severity::Error,
            "step 2 (keep-matching) left no selections (after select-nodes 9); nothing changed"
                .into()
        )
    );
    assert_eq!(ed.view().selection, before);
    assert_eq!(text(&ed), FIXTURE);

    command(
        &mut ed,
        "select-nodes (struct_item name: (type_identifier) @n (#eq? @n \"Nope\")) @select",
    );
    assert_eq!(
        status(&ed).1,
        "select-nodes left no selections; nothing changed"
    );
    assert_eq!(ed.view().selection, before);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn filters_by_text_and_by_contained_nodes() {
    let dir = temp_dir("filters");
    let mut ed = open(&dir.join("service.rs"), FIXTURE);
    command(&mut ed, "select-nodes fn | keep-matching -> Result<\\(\\)");
    assert_eq!(
        fn_names(&ed),
        [
            "fetch",
            "already_traced",
            "also_traced",
            "no_result",
            "sync_one",
            "private_one",
            "crate_one",
            "method",
            "traced_method",
            "private_method"
        ],
        "regexes see all the text: fetch's string literal and no_result's nested helper match"
    );
    command(&mut ed, "select-nodes fn | remove-matching (async|helper)");
    assert_eq!(fn_names(&ed), ["sync_one", "traced_private"]);
    command(&mut ed, "select-nodes fn | keep-containing string");
    assert_eq!(fn_names(&ed), ["fetch"]);
    command(
        &mut ed,
        "select-nodes fn | remove-containing call | keep-matching traced",
    );
    assert_eq!(fn_names(&ed), ["traced_private"]);
    assert_eq!(
        status(&ed).1,
        "select-nodes 14 (1 nested or overlapping merged) | remove-containing 2 | keep-matching 1"
    );
    // A Rust function's node does not include the attributes above it, so
    // its text does not either.
    command(
        &mut ed,
        "select-nodes fn | remove-containing call | keep-matching instrument",
    );
    assert_eq!(
        status(&ed).1,
        "step 3 (keep-matching) left no selections (after select-nodes 14 (1 nested or overlapping merged) | remove-containing 2); nothing changed"
    );
    assert_eq!(fn_names(&ed), ["traced_private"]);
    // keep-nodes tests the selection's own node; keep-containing any node
    // inside it, so a nested function counts.
    command(
        &mut ed,
        "select-nodes pub-async-fn | keep-nodes returns-result",
    );
    assert!(!fn_names(&ed).contains(&"no_result".to_string()));
    command(
        &mut ed,
        "select-nodes pub-async-fn | keep-containing returns-result",
    );
    assert!(fn_names(&ed).contains(&"no_result".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn expand_to_and_select_inside_walk_the_tree() {
    let dir = temp_dir("expand");
    let mut ed = open(&dir.join("service.rs"), FIXTURE);
    // Cursor on `todo` inside fetch.
    let at = FIXTURE.find("todo!").unwrap();
    ed.set_selection(maestro_core::Selection::point(at));
    command(&mut ed, "expand-to function_item");
    assert_eq!(fn_names(&ed), ["fetch"]);
    assert_eq!(status(&ed).1, "expand-to 1");

    // Every call (all `Ok(..)`), grown to its function: one per function,
    // the two calls of nothing merged, and helper's call merges into
    // helper, which sits inside no_result.
    command(&mut ed, "select-nodes call | expand-to function_item");
    assert_eq!(
        fn_names(&ed),
        [
            "already_traced",
            "also_traced",
            "helper",
            "sync_one",
            "private_one",
            "crate_one",
            "io_result",
            "after_traced_private",
            "deep",
            "method",
            "traced_method",
            "private_method"
        ]
    );
    // Selections with no enclosing node of the kind are dropped.
    command(&mut ed, "select-nodes call | expand-to impl_item");
    assert_eq!(selections(&ed).len(), 1);
    assert!(selections(&ed)[0].starts_with("impl Service {"));
    // Then the calls inside that impl only.
    command(&mut ed, "select-inside call");
    assert_eq!(selections(&ed), ["Ok(())", "Ok(())", "Ok(())"]);
    assert_eq!(status(&ed).1, "select-inside 3");
    // Several kinds: the smallest enclosing one of either.
    command(
        &mut ed,
        "select-nodes string | expand-to impl_item function_item",
    );
    assert_eq!(fn_names(&ed), ["fetch"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nested_matches_merge_and_the_status_says_so() {
    let dir = temp_dir("nested");
    let mut ed = open(&dir.join("service.rs"), FIXTURE);
    command(&mut ed, "select-nodes fn");
    // 15 function items (a trait's signature and a macro body are not),
    // helper merged into no_result.
    assert_eq!(
        status(&ed).1,
        "select-nodes 14 (1 nested or overlapping merged)"
    );
    assert!(!fn_names(&ed).contains(&"helper".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn insert_above_refuses_a_selection_that_starts_mid_line() {
    let dir = temp_dir("midline");
    let src = "impl X { pub async fn a() -> Result<(), ()> { Ok(()) } }\n\npub async fn b() -> Result<(), ()> {\n    Ok(())\n}\n";
    let mut ed = open(&dir.join("x.rs"), src);
    command(
        &mut ed,
        "select-nodes pub-async-fn | insert-above #[tracing::instrument]",
    );
    assert_eq!(
        status(&ed),
        (
            Severity::Error,
            "insert-above: the selection on line 1 starts after other text on that line, so a line above it would not belong to it; nothing changed".into()
        )
    );
    assert_eq!(text(&ed), src);
    assert!(!ed.doc().is_modified());

    // Whole-line selections (`x`) start at column 0, inside the
    // indentation, which is fine.
    command(&mut ed, "select-nodes call");
    run(&mut ed, "x");
    command(&mut ed, "insert-above // calls Ok");
    assert_eq!(
        text(&ed),
        "// calls Ok\nimpl X { pub async fn a() -> Result<(), ()> { Ok(()) } }\n\npub async fn b() -> Result<(), ()> {\n    // calls Ok\n    Ok(())\n}\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn insert_above_keeps_crlf_and_tab_indentation_and_one_line_per_line() {
    let dir = temp_dir("crlf");
    let src = "mod m {\r\n\tpub async fn a() -> Result<(), ()> { Ok(()) } pub async fn b() -> Result<(), ()> { Ok(()) }\r\n}\r\n";
    let mut ed = open(&dir.join("x.rs"), src);
    // Two selections on one line: only the first starts the line, so the
    // second is refused.
    command(
        &mut ed,
        "select-nodes pub-async-fn | insert-above #[inline]",
    );
    assert_eq!(status(&ed).0, Severity::Error);
    // As whole lines, both selections are the same line, which gets one
    // new line.
    command(&mut ed, "select-nodes pub-async-fn");
    assert_eq!(selections(&ed).len(), 2);
    run(&mut ed, "x");
    command(&mut ed, "insert-above #[inline]");
    assert_eq!(
        text(&ed),
        "mod m {\r\n\t#[inline]\r\n\tpub async fn a() -> Result<(), ()> { Ok(()) } pub async fn b() -> Result<(), ()> { Ok(()) }\r\n}\r\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_refused_edit_leaves_the_selections_too() {
    let dir = temp_dir("readonly");
    let path = dir.join("bad.rs");
    // Invalid UTF-8 opens read-only.
    std::fs::write(
        &path,
        b"pub async fn a() -> Result<(), ()> {\n    Ok(())\n}\n// \xff\n",
    )
    .unwrap();
    let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 120, 40));
    ed.open(&path).unwrap();
    settle(&mut ed);
    let before_text = text(&ed);
    let before = ed.view().selection.clone();
    command(&mut ed, SPEC_PIPELINE);
    assert_eq!(status(&ed).0, Severity::Error);
    assert!(
        status(&ed).1.starts_with("document is read-only"),
        "{:?}",
        status(&ed)
    );
    assert_eq!(text(&ed), before_text);
    assert_eq!(ed.view().selection, before);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn python_functions_get_a_decorator() {
    let dir = temp_dir("python");
    let src = "import os\n\n\ndef keep(a):\n    return os.path.join(a, 'x')\n\n\nasync def fetch(url):\n    return await get(url)\n\n\nclass K:\n    @staticmethod\n    def m():\n        pass\n";
    let mut ed = open(&dir.join("x.py"), src);
    command(
        &mut ed,
        "select-nodes fn | keep-containing call | insert-above @traced",
    );
    assert_eq!(
        status(&ed).1,
        "select-nodes 3 | keep-containing 2 | insert-above 2: inserted 2 lines (u undoes)"
    );
    assert_eq!(
        text(&ed),
        "import os\n\n\n@traced\ndef keep(a):\n    return os.path.join(a, 'x')\n\n\n@traced\nasync def fetch(url):\n    return await get(url)\n\n\nclass K:\n    @staticmethod\n    def m():\n        pass\n"
    );
    run(&mut ed, "u");
    assert_eq!(text(&ed), src);
    command(&mut ed, "select-nodes async-fn");
    assert_eq!(selections(&ed).len(), 1);
    assert!(selections(&ed)[0].starts_with("async def fetch"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn buffers_without_a_tree_say_why() {
    let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 80, 24));
    run(&mut ed, "ifn a() {}<esc>");
    command(&mut ed, "select-nodes fn");
    assert_eq!(
        status(&ed),
        (
            Severity::Error,
            "select-nodes: no syntax tree: file type not recognized".into()
        )
    );
    // Text steps work without a tree.
    run(&mut ed, "%");
    command(&mut ed, "keep-matching fn | insert-above // hi");
    assert_eq!(text(&ed), "// hi\nfn a() {}");
    // A step run by name alone (as `command.run` does) reports the missing
    // argument.
    ed.run_command("select-nodes");
    assert_eq!(
        status(&ed).1,
        "select-nodes: no syntax tree: file type not recognized"
    );
    ed.run_command("insert-above");
    assert_eq!(status(&ed).1, "insert-above: needs the text to insert");
}

/// A generated Rust file of `lines` lines with the fixture's kinds of
/// functions repeated, and how many functions the spec pipeline should
/// pick.
fn large_rust(lines: usize) -> (String, usize) {
    let mut out = String::from("use std::io;\n\npub struct Error;\n\n");
    let mut expected = 0;
    let mut i = 0;
    while out.lines().count() < lines {
        let block = match i % 4 {
            0 => {
                expected += 1;
                format!(
                    "pub async fn f{i}(x: u8) -> Result<u8, Error> {{\n    let y = x + 1;\n    Ok(y)\n}}\n\n"
                )
            }
            1 => format!(
                "#[tracing::instrument]\npub async fn f{i}() -> Result<(), Error> {{\n    Ok(())\n}}\n\n"
            ),
            2 => format!("pub fn f{i}() -> io::Result<()> {{\n    Ok(())\n}}\n\n"),
            _ => format!(
                "pub async fn f{i}() -> u32 {{\n    let s = \"pub async fn x() -> Result<(), Error>\";\n    s.len() as u32\n}}\n\n"
            ),
        };
        out.push_str(&block);
        i += 1;
    }
    (out, expected)
}

#[test]
fn the_spec_pipeline_on_a_large_file() {
    let dir = temp_dir("large");
    let (src, expected) = large_rust(10_000);
    let mut ed = open(&dir.join("big.rs"), &src);
    command(&mut ed, SPEC_PIPELINE);
    let (sev, msg) = status(&ed);
    assert_eq!(sev, Severity::Info, "{msg}");
    assert!(
        msg.ends_with(&format!("inserted {expected} lines (u undoes)")),
        "{msg}"
    );
    assert_eq!(
        text(&ed).matches("#[tracing::instrument]\n").count(),
        src.matches("#[tracing::instrument]\n").count() + expected
    );
    run(&mut ed, "u");
    assert_eq!(text(&ed), src);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Times the spec pipeline on a 10,000-line file. Run with
/// `cargo test --release -p maestro-editor --test pipelines -- --ignored --nocapture`.
#[test]
#[ignore = "timing; run by hand in release"]
fn measure_spec_pipeline_on_10k_lines() {
    let dir = temp_dir("measure");
    let (src, expected) = large_rust(10_000);
    let mut ed = open(&dir.join("big.rs"), &src);
    let lines = src.lines().count();
    let mut select_only = Vec::new();
    let mut full = Vec::new();
    for _ in 0..21 {
        // Each timing covers Enter on the typed line: parsing the steps,
        // the queries, the selections and (for the full pipeline) the
        // transaction and the reparse it triggers on the main thread.
        let line =
            "select-nodes pub-async-fn | keep-nodes returns-result | remove-nodes instrumented";
        run(&mut ed, ":");
        for c in line.chars() {
            ed.handle_key(KeyEvent::char(c));
        }
        let t = Instant::now();
        ed.handle_key(KeyEvent::plain(KeyCode::Enter));
        select_only.push(t.elapsed());
        assert_eq!(ed.view().selection.len(), expected);

        run(&mut ed, ":");
        for c in SPEC_PIPELINE.chars() {
            ed.handle_key(KeyEvent::char(c));
        }
        let t = Instant::now();
        ed.handle_key(KeyEvent::plain(KeyCode::Enter));
        full.push(t.elapsed());
        assert_eq!(status(&ed).0, Severity::Info);
        run(&mut ed, "u");
        settle(&mut ed);
        assert_eq!(text(&ed), src);
    }
    select_only.sort();
    full.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!(
        "{lines} lines, {} bytes, {expected} matches, 21 runs each: selection steps median {:.2} ms (min {:.2}, max {:.2}); full pipeline with insert median {:.2} ms (min {:.2}, max {:.2})",
        src.len(),
        ms(select_only[10]),
        ms(select_only[0]),
        ms(select_only[20]),
        ms(full[10]),
        ms(full[0]),
        ms(full[20]),
    );
    let _ = std::fs::remove_dir_all(&dir);
}
