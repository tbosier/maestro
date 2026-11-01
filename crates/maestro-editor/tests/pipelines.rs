//! Structural pipelines (`:select-nodes`, filters, `:expand-to`, and the
//! text steps `:insert-above`, `:insert-below`, `:replace-text`, `:delete`,
//! `:wrap`, `:prepend`, `:append`), driven through keys.

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

/// The raw-query pipeline as docs/editor-model.md prints it.
fn documented_raw_pipeline() -> String {
    let doc = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/editor-model.md"),
    )
    .unwrap();
    doc.lines()
        .find_map(|l| l.strip_prefix(":select-nodes (function_item"))
        .map(|rest| format!("select-nodes (function_item{rest}"))
        .expect("the raw pipeline is in the docs")
}

#[test]
fn the_documented_raw_queries_give_the_same_result_as_the_shortcuts() {
    let raw = documented_raw_pipeline();
    let dir = temp_dir("raw");
    let mut ed = open(&dir.join("service.rs"), FIXTURE);
    command(&mut ed, &raw);
    let (sev, msg) = status(&ed);
    assert_eq!(sev, Severity::Info, "{msg}");
    assert_eq!(text(&ed), expected_after_spec_pipeline());

    // Return types the fixture lacks: a plain `Result` alias and paths.
    let src = "type Result = std::result::Result<(), ()>;\npub async fn a() -> Result {\n    Ok(())\n}\npub async fn b() -> io::Result<()> {\n    Ok(())\n}\npub async fn c() -> std::io::Result<()> {\n    Ok(())\n}\npub async fn d() -> Option<Result> {\n    None\n}\n";
    let mut results = Vec::new();
    for line in [raw.as_str(), SPEC_PIPELINE] {
        let mut ed = open(&dir.join("alias.rs"), src);
        command(&mut ed, line);
        assert_eq!(status(&ed).0, Severity::Info, "{:?}", status(&ed));
        results.push(text(&ed));
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(results[0].matches("#[tracing::instrument]").count(), 3);
    assert!(!results[0].contains("#[tracing::instrument]\npub async fn d"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_queries_report_the_position_and_change_nothing() {
    let dir = temp_dir("invalid");
    let mut ed = open(&dir.join("service.rs"), FIXTURE);
    let before = ed.view().selection.clone();

    // Errors name the step and the column on the line (one-based chars).
    command(&mut ed, "select-nodes (function_item (nope))");
    assert_eq!(
        status(&ed),
        (
            Severity::Error,
            "select-nodes, column 30: invalid query: unknown node kind \"nope\"".into()
        )
    );
    command(&mut ed, "select-nodes (function_item");
    let (sev, msg) = status(&ed);
    assert_eq!(sev, Severity::Error);
    assert!(msg.starts_with("select-nodes, column "), "{msg}");
    assert!(msg.contains("invalid query: syntax error"), "{msg}");

    // A bad third step stops the pipeline before the first one runs.
    let line = "select-nodes fn | keep-nodes returns-result | remove-nodes (attribute_item (bogus)) | insert-above x";
    command(&mut ed, line);
    assert_eq!(
        status(&ed),
        (
            Severity::Error,
            format!(
                "step 3 (remove-nodes), column {}: invalid query: unknown node kind \"bogus\"",
                line.find("bogus").unwrap() + 1
            )
        )
    );
    command(&mut ed, "select-nodes nonsense");
    let (_, msg) = status(&ed);
    assert!(
        msg.starts_with(
            "select-nodes, column 14: no rust shortcut named \"nonsense\" (shortcuts: fn, pub-fn,"
        ),
        "{msg}"
    );
    command(&mut ed, "expand-to function_item fn");
    let (_, msg) = status(&ed);
    assert!(
        msg.starts_with("expand-to, column 25: rust has no node kind \"fn\""),
        "{msg}"
    );
    command(&mut ed, "keep-matching (unclosed");
    let (_, msg) = status(&ed);
    assert!(
        msg.starts_with("keep-matching, column 15: invalid regex"),
        "{msg}"
    );
    command(
        &mut ed,
        "select-nodes fn | insert-above x | keep-matching y",
    );
    assert_eq!(
        status(&ed).1,
        "step 2 (insert-above), column 19: insert-above changes the text, so it must be the last step"
    );
    let line = r#"select-nodes (function_item name: (identifier) @n (#equal? @n "fetch")) @select | insert-above #[inline]"#;
    command(&mut ed, line);
    assert!(
        status(&ed).1.starts_with(&format!(
            "step 1 (select-nodes), column {}: invalid query: unsupported predicate #equal?",
            line.find("#equal?").unwrap() + 1
        )),
        "{:?}",
        status(&ed)
    );
    ed.execute_line("select-nodes fn | insert-above // one\n// two");
    assert_eq!(
        status(&ed).1,
        "step 2 (insert-above), column 32: takes one line of text, without line breaks"
    );
    command(&mut ed, "select-nodes fn | | keep-matching y");
    assert_eq!(status(&ed).1, "step 2, column 19: the step is empty");
    command(&mut ed, "select-nodes fn | write");
    assert!(
        status(&ed)
            .1
            .starts_with("step 2 (write), column 19: write cannot be a pipeline step"),
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
        "select-nodes 15 | remove-containing 2 | keep-matching 1"
    );
    // A Rust function's node does not include the attributes above it, so
    // its text does not either.
    command(
        &mut ed,
        "select-nodes fn | remove-containing call | keep-matching instrument",
    );
    assert_eq!(
        status(&ed).1,
        "step 3 (keep-matching) left no selections (after select-nodes 15 | remove-containing 2); nothing changed"
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
    // 15 function items (a trait's signature and a macro body are not);
    // helper merges into no_result only when the selection is made.
    assert_eq!(
        status(&ed).1,
        "select-nodes 15; 14 selections after merging 1 nested in others"
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
    assert_eq!(
        status(&ed).1,
        "insert-above, column 13: needs the text to insert"
    );
}

#[test]
fn a_function_nested_in_one_the_filters_drop_still_gets_the_attribute() {
    let dir = temp_dir("nested-kept");
    let src = "pub async fn outer() -> u32 {\n    pub async fn inner() -> Result<(), ()> {\n        Ok(())\n    }\n    1\n}\n\npub async fn sibling() -> Result<(), ()> {\n    Ok(())\n}\n";
    let mut ed = open(&dir.join("x.rs"), src);
    command(&mut ed, SPEC_PIPELINE);
    assert_eq!(
        status(&ed).1,
        "select-nodes 3 | keep-nodes 2 | remove-nodes 2 | insert-above 2: inserted 2 lines (u undoes)"
    );
    assert_eq!(
        text(&ed),
        "pub async fn outer() -> u32 {\n    #[tracing::instrument]\n    pub async fn inner() -> Result<(), ()> {\n        Ok(())\n    }\n    1\n}\n\n#[tracing::instrument]\npub async fn sibling() -> Result<(), ()> {\n    Ok(())\n}\n"
    );
    assert_eq!(fn_names(&ed), ["inner", "sibling"]);
    // When both the outer and the inner function survive, both get the
    // line, and the selection merges them.
    run(&mut ed, "u");
    command(&mut ed, "select-nodes pub-async-fn | insert-above // async");
    assert_eq!(
        status(&ed).1,
        "select-nodes 3 | insert-above 3: inserted 3 lines (u undoes); 2 selections after merging 1 nested in others"
    );
    assert_eq!(text(&ed).matches("// async\n").count(), 3);
    assert_eq!(fn_names(&ed), ["outer", "sibling"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn insert_above_treats_only_newline_as_a_line_break() {
    let dir = temp_dir("separators");
    for sep in ['\u{2028}', '\u{2029}', '\r', '\u{0b}', '\u{0c}', '\u{85}'] {
        let src = format!("// disabled{sep}fn disabled() {{}}\nfn real() {{}}\n");
        let mut ed = open(&dir.join("x.rs"), &src);
        // The commented-out function is not a node.
        command(&mut ed, "select-nodes fn");
        assert_eq!(selections(&ed), ["fn real() {}"], "{sep:?}");
        // A cursor on it, after the separator, is mid-line for a compiler.
        let at = src.chars().position(|c| c == sep).unwrap() + 1;
        ed.set_selection(maestro_core::Selection::point(at));
        command(&mut ed, "insert-above // note");
        assert_eq!(status(&ed).0, Severity::Error, "{sep:?}");
        assert!(status(&ed).1.contains("starts after other text"), "{sep:?}");
        assert_eq!(text(&ed), src, "{sep:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pipeline_run_during_an_insert_session_is_its_own_undo_step() {
    let dir = temp_dir("insert-undo");
    let mut ed = open(&dir.join("x.rs"), "fn f() {}\n");
    run(&mut ed, "i\n");
    assert_eq!(text(&ed), "\nfn f() {}\n");
    // As the automation API's editor.execute does, mid-insert.
    ed.execute_line("select-nodes fn | insert-above // hi");
    assert_eq!(text(&ed), "\n// hi\nfn f() {}\n");
    run(&mut ed, "<esc>u");
    assert_eq!(text(&ed), "\nfn f() {}\n", "the typed newline stays");
    run(&mut ed, "u");
    assert_eq!(text(&ed), "fn f() {}\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn regex_filters_count_empty_matches() {
    let dir = temp_dir("zero-width");
    let mut ed = open(&dir.join("x.rs"), "fn f() {}\n");
    command(&mut ed, "select-nodes fn | keep-matching ^");
    assert_eq!(status(&ed).1, "select-nodes 1 | keep-matching 1");
    command(
        &mut ed,
        "select-nodes fn | remove-matching ^ | insert-above #[inline]",
    );
    assert_eq!(
        status(&ed).1,
        "step 2 (remove-matching) left no selections (after select-nodes 1); nothing changed"
    );
    assert_eq!(text(&ed), "fn f() {}\n");
    // A character class with a paren does not swallow the next step.
    command(
        &mut ed,
        "select-nodes fn | remove-matching [(] | insert-above #[inline]",
    );
    assert_eq!(
        status(&ed).1,
        "step 2 (remove-matching) left no selections (after select-nodes 1); nothing changed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replace_text_uses_capture_groups_inside_the_selections_only() {
    let dir = temp_dir("replace");
    let src = "fn load_old(a: u32) -> u32 {\n    helper_old(a) + helper_old(1)\n}\n\nfn other() {}\n// helper_old stays\n";
    let mut ed = open(&dir.join("x.rs"), src);
    command(
        &mut ed,
        r#"select-nodes fn | replace-text "(\w+)_old" "${1}_new""#,
    );
    assert_eq!(
        status(&ed),
        (
            Severity::Info,
            "select-nodes 2 | replace-text 2: replaced 3 matches in 1 selection (u undoes)".into()
        )
    );
    assert_eq!(
        text(&ed),
        "fn load_new(a: u32) -> u32 {\n    helper_new(a) + helper_new(1)\n}\n\nfn other() {}\n// helper_old stays\n"
    );
    run(&mut ed, "u");
    assert_eq!(text(&ed), src);
    // Named groups; bare arguments are taken literally.
    command(
        &mut ed,
        r#"select-nodes fn | replace-text (?P<base>\w+)_old $base"#,
    );
    assert!(text(&ed).starts_with("fn load(a: u32)"), "{}", text(&ed));
    run(&mut ed, "u");
    // Smart case: an uppercase letter makes the regex case-sensitive, so
    // this matches nothing and nothing changes.
    command(&mut ed, "select-nodes fn | replace-text OLD new");
    assert_eq!(
        status(&ed),
        (
            Severity::Error,
            "step 2 (replace-text) changed nothing (replaced 0 matches in 0 selections); nothing changed".into()
        )
    );
    assert_eq!(text(&ed), src);
    // An empty match inserts: a prefix on every line of the selection.
    command(
        &mut ed,
        r#"select-nodes fn | keep-matching load | replace-text ^ "// ""#,
    );
    assert_eq!(
        text(&ed),
        "// fn load_old(a: u32) -> u32 {\n//     helper_old(a) + helper_old(1)\n// }\n\nfn other() {}\n// helper_old stays\n"
    );
    run(&mut ed, "u");
    assert_eq!(text(&ed), src);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn delete_wrap_prepend_and_append_change_each_selection_as_one_undo_step() {
    let dir = temp_dir("edits");
    let src = "fn a() {\n    let s = \"x\";\n    let t = \"y\";\n}\nfn b() {}\n";
    let mut ed = open(&dir.join("x.rs"), src);

    command(&mut ed, "select-nodes string | delete");
    assert_eq!(
        status(&ed).1,
        "select-nodes 2 | delete 2: deleted 2 selections (u undoes)"
    );
    assert_eq!(
        text(&ed),
        "fn a() {\n    let s = ;\n    let t = ;\n}\nfn b() {}\n"
    );
    run(&mut ed, "u");
    assert_eq!(text(&ed), src);

    command(&mut ed, r#"select-nodes string | wrap "String::from(" )"#);
    assert_eq!(
        status(&ed).1,
        "select-nodes 2 | wrap 2: wrapped 2 selections (u undoes)"
    );
    assert_eq!(
        text(&ed),
        "fn a() {\n    let s = String::from(\"x\");\n    let t = String::from(\"y\");\n}\nfn b() {}\n"
    );
    // The selections still cover the strings, not the added text.
    assert_eq!(selections(&ed), ["\"x\"", "\"y\""]);
    run(&mut ed, "u");
    assert_eq!(text(&ed), src);

    command(&mut ed, r#"select-nodes fn | prepend "pub ""#);
    assert_eq!(
        status(&ed).1,
        "select-nodes 2 | prepend 2: prepended to 2 selections (u undoes)"
    );
    assert!(text(&ed).starts_with("pub fn a() {"));
    assert!(text(&ed).ends_with("pub fn b() {}\n"));
    assert!(selections(&ed).iter().all(|s| s.starts_with("fn ")));
    run(&mut ed, "u");
    assert_eq!(text(&ed), src);

    command(
        &mut ed,
        r#"select-nodes fn | keep-matching b | append " // b""#,
    );
    assert_eq!(
        status(&ed).1,
        "select-nodes 2 | keep-matching 1 | append 1: appended to 1 selection (u undoes)"
    );
    assert!(text(&ed).ends_with("fn b() {} // b\n"));
    assert_eq!(selections(&ed), ["fn b() {}"]);
    run(&mut ed, "u");
    assert_eq!(text(&ed), src);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn insert_below_follows_the_last_line_with_the_first_lines_indentation() {
    let dir = temp_dir("below");
    let src = "mod m {\n    fn a() {\n        x();\n    }\n    fn b() {} fn c() {}\n}\n";
    let mut ed = open(&dir.join("x.rs"), src);
    command(
        &mut ed,
        "select-nodes fn | keep-matching fn a | insert-below // after a",
    );
    assert_eq!(
        status(&ed).1,
        "select-nodes 3 | keep-matching 1 | insert-below 1: inserted 1 line (u undoes)"
    );
    assert_eq!(
        text(&ed),
        "mod m {\n    fn a() {\n        x();\n    }\n    // after a\n    fn b() {} fn c() {}\n}\n"
    );
    run(&mut ed, "u");
    // A selection that ends before other text on its line is refused.
    command(
        &mut ed,
        "select-nodes fn | keep-matching fn b | insert-below // x",
    );
    assert_eq!(
        status(&ed),
        (
            Severity::Error,
            "insert-below: the selection on line 5 ends before other text on that line, so a line below it would not belong to it; nothing changed".into()
        )
    );
    assert_eq!(text(&ed), src);
    // As a whole line (`x`), b and c share one new line.
    command(&mut ed, "select-nodes fn | keep-matching fn [bc]");
    run(&mut ed, "x");
    command(&mut ed, "insert-below // two");
    assert_eq!(
        text(&ed),
        "mod m {\n    fn a() {\n        x();\n    }\n    fn b() {} fn c() {}\n    // two\n}\n"
    );

    // CRLF, and a last line without a line break.
    let mut ed = open(&dir.join("crlf.rs"), "fn a() {}\r\nfn b() {}");
    command(&mut ed, "select-nodes fn | insert-below // x");
    assert_eq!(text(&ed), "fn a() {}\r\n// x\r\nfn b() {}\r\n// x");
    run(&mut ed, "u");
    assert_eq!(text(&ed), "fn a() {}\r\nfn b() {}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn text_step_arguments_quote_and_escape_simply() {
    let dir = temp_dir("quoting");
    let src = "fn a() {}\n";
    let mut ed = open(&dir.join("x.rs"), src);
    // A bar inside quotes is text, not a new step.
    command(&mut ed, r#"select-nodes fn | append " | x""#);
    assert_eq!(text(&ed), "fn a() {} | x\n");
    run(&mut ed, "u");
    // \n and \t in quotes; a bare text keeps its backslashes.
    command(&mut ed, r#"select-nodes fn | prepend "// one\n""#);
    assert_eq!(text(&ed), "// one\nfn a() {}\n");
    run(&mut ed, "u");
    command(&mut ed, r"select-nodes fn | append \t");
    assert_eq!(text(&ed), "fn a() {}\\t\n");
    run(&mut ed, "u");
    assert_eq!(text(&ed), src);

    let refused = [
        (
            r#"select-nodes fn | append "open"#,
            r#"step 2 (append), column 26: this quoted text has no closing ""#.to_string(),
        ),
        (
            "select-nodes fn | prepend x | append y",
            "step 2 (prepend), column 19: prepend changes the text, so it must be the last step".to_string(),
        ),
        (
            "select-nodes fn | delete now",
            "step 2 (delete), column 26: takes no argument".to_string(),
        ),
        (
            "select-nodes fn | wrap a b c",
            "step 2 (wrap), column 28: takes two arguments (<before> <after>); quote an argument that has spaces".to_string(),
        ),
        (
            "select-nodes fn | wrap a",
            "step 2 (wrap), column 25: needs two arguments: <before> <after>".to_string(),
        ),
        (
            r#"select-nodes fn | wrap "" """#,
            "step 2 (wrap), column 24: needs a text before or after".to_string(),
        ),
        (
            // As printed in docs/editor-model.md.
            "wrap x",
            "wrap, column 7: needs two arguments: <before> <after>".to_string(),
        ),
        (
            r#"select-nodes fn | insert-below "a\nb""#,
            "step 2 (insert-below), column 32: takes one line of text, without line breaks".to_string(),
        ),
        (
            r#"select-nodes fn | prepend """#,
            "step 2 (prepend), column 27: needs the text to insert".to_string(),
        ),
        (
            r#"select-nodes fn | append "a" b"#,
            "step 2 (append), column 30: takes one text; put the whole text inside the quotes".to_string(),
        ),
    ];
    for (line, want) in refused {
        command(&mut ed, line);
        assert_eq!(status(&ed), (Severity::Error, want), "{line}");
        assert_eq!(text(&ed), src, "{line}");
    }
    let line = r#"select-nodes fn | replace-text "(a)" "$2""#;
    command(&mut ed, line);
    let (sev, msg) = status(&ed);
    assert_eq!(sev, Severity::Error);
    assert!(
        msg.starts_with(&format!(
            "step 2 (replace-text), column {}: the replacement refers to group $2",
            line.find("\"$2").unwrap() + 1
        )),
        "{msg}"
    );
    let line = r#"select-nodes fn | replace-text "(" x"#;
    command(&mut ed, line);
    assert!(
        status(&ed)
            .1
            .starts_with("step 2 (replace-text), column 32: invalid regex"),
        "{:?}",
        status(&ed)
    );
    assert_eq!(text(&ed), src);
    assert!(!ed.doc().is_modified());
    let _ = std::fs::remove_dir_all(&dir);
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
