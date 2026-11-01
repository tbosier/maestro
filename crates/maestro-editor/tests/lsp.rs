//! Language server behavior tests: keys in through `Editor::handle_key`,
//! a real child process (the fake server) on the other end.
//!
//! `harness = false`: started with `--maestro-fake-lsp`, this binary is the
//! fake server instead (see `maestro_lsp::fake`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use maestro_config::{Config, LanguageServerConfig};
use maestro_editor::lsp::{self, DiagnosticSeverity, completion};
use maestro_editor::{Editor, KeyCode, KeyEvent, Mode, Rect};
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
        (
            "reply_dropped_when_another_buffer_changed",
            reply_dropped_when_another_buffer_changed,
        ),
        (
            "hover_dropped_when_the_cursor_moved",
            hover_dropped_when_the_cursor_moved,
        ),
        ("save_sends_text_when_asked", save_sends_text_when_asked),
        (
            "definition_reply_dropped_while_a_prompt_is_open",
            definition_reply_dropped_while_a_prompt_is_open,
        ),
        (
            "completion_on_demand_filters_and_accepts",
            completion_on_demand_filters_and_accepts,
        ),
        (
            "completion_after_typing_waits_for_the_debounce",
            completion_after_typing_waits_for_the_debounce,
        ),
        (
            "completion_on_a_trigger_character",
            completion_on_a_trigger_character,
        ),
        (
            "completion_escape_and_motion_close_the_popup",
            completion_escape_and_motion_close_the_popup,
        ),
        (
            "completion_insert_replace_edit_uses_the_insert_range",
            completion_insert_replace_edit_uses_the_insert_range,
        ),
        (
            "completion_insert_text_and_label_only_items",
            completion_insert_text_and_label_only_items,
        ),
        (
            "completion_additional_edits_apply_in_one_transaction",
            completion_additional_edits_apply_in_one_transaction,
        ),
        (
            "completion_snippet_puts_the_cursor_at_the_final_stop",
            completion_snippet_puts_the_cursor_at_the_final_stop,
        ),
        (
            "completion_utf16_positions_with_emoji_and_cjk",
            completion_utf16_positions_with_emoji_and_cjk,
        ),
        (
            "completion_stale_replies_are_dropped",
            completion_stale_replies_are_dropped,
        ),
        (
            "completion_at_every_cursor_where_the_text_matches",
            completion_at_every_cursor_where_the_text_matches,
        ),
        (
            "completion_incomplete_list_asks_again",
            completion_incomplete_list_asks_again,
        ),
        (
            "completion_dot_repeats_an_accepted_item",
            completion_dot_repeats_an_accepted_item,
        ),
        (
            "completion_in_a_crlf_document_inserts_crlf",
            completion_in_a_crlf_document_inserts_crlf,
        ),
        (
            "completion_that_skipped_a_cursor_is_not_repeated",
            completion_that_skipped_a_cursor_is_not_repeated,
        ),
        (
            "completion_item_dropped_when_its_import_spot_is_edited",
            completion_item_dropped_when_its_import_spot_is_edited,
        ),
        (
            "completion_closes_when_a_secondary_cursor_moves",
            completion_closes_when_a_secondary_cursor_moves,
        ),
        (
            "completion_debounce_does_not_follow_a_click_to_another_view",
            completion_debounce_does_not_follow_a_click_to_another_view,
        ),
        (
            "completion_keys_pass_through_when_the_popup_cannot_be_drawn",
            completion_keys_pass_through_when_the_popup_cannot_be_drawn,
        ),
        (
            "rename_prompt_is_prefilled_and_renames_across_files",
            rename_prompt_is_prefilled_and_renames_across_files,
        ),
        (
            "rename_without_prepare_and_as_a_typed_command",
            rename_without_prepare_and_as_a_typed_command,
        ),
        (
            "rename_refuses_bad_edits_and_changes_nothing",
            rename_refuses_bad_edits_and_changes_nothing,
        ),
        (
            "rename_reports_server_errors_and_unrenamable_spots",
            rename_reports_server_errors_and_unrenamable_spots,
        ),
        (
            "rename_reply_dropped_after_typing",
            rename_reply_dropped_after_typing,
        ),
        (
            "rename_accepts_the_changes_form",
            rename_accepts_the_changes_form,
        ),
        (
            "format_is_one_undo_step_and_keeps_the_cursor",
            format_is_one_undo_step_and_keeps_the_cursor,
        ),
        (
            "format_whole_text_reply_keeps_the_cursor_line",
            format_whole_text_reply_keeps_the_cursor_line,
        ),
        (
            "format_selection_formats_only_the_selected_lines",
            format_selection_formats_only_the_selected_lines,
        ),
        (
            "format_errors_and_stale_replies_change_nothing",
            format_errors_and_stale_replies_change_nothing,
        ),
        (
            "code_action_quick_fix_sends_diagnostics_and_runs_its_command",
            code_action_quick_fix_sends_diagnostics_and_runs_its_command,
        ),
        (
            "code_action_resolve_and_server_edit_request",
            code_action_resolve_and_server_edit_request,
        ),
        (
            "code_action_failures_and_refused_server_edit",
            code_action_failures_and_refused_server_edit,
        ),
        (
            "code_action_stale_list_and_stale_choice_change_nothing",
            code_action_stale_list_and_stale_choice_change_nothing,
        ),
        (
            "signature_help_follows_the_parameter_and_closes",
            signature_help_follows_the_parameter_and_closes,
        ),
        (
            "signature_help_on_demand_and_leaving_insert_mode",
            signature_help_on_demand_and_leaving_insert_mode,
        ),
        (
            "signature_help_stale_replies_are_not_shown",
            signature_help_stale_replies_are_not_shown,
        ),
        (
            "rename_reply_dropped_when_a_file_is_opened_while_waiting",
            rename_reply_dropped_when_a_file_is_opened_while_waiting,
        ),
        (
            "rename_prepare_dropped_when_the_cursor_moves",
            rename_prepare_dropped_when_the_cursor_moves,
        ),
        (
            "command_edit_refused_after_typing",
            command_edit_refused_after_typing,
        ),
        (
            "code_action_is_not_repeated_or_recorded",
            code_action_is_not_repeated_or_recorded,
        ),
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
    fake_config_with(&[])
}

fn fake_config_with(extra: &[&str]) -> Config {
    let mut config = Config::default();
    let mut args = vec![FAKE_ARG.to_string()];
    args.extend(extra.iter().map(|a| a.to_string()));
    config.language_server.insert(
        "rust".into(),
        LanguageServerConfig {
            command: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args,
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
    open_synced_with(name, text, fake_config())
}

fn open_synced_with(name: &str, text: &str, config: Config) -> Fixture {
    let mut f = open(name, text, config);
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
        // Unindenting an unindented line changes nothing: no didChange may
        // repeat a version (the fake server records that as an error).
        ("no-op unindent", "gg<lt>"),
    ] {
        run(ed, script);
        assert_server_matches(ed, step);
    }
    // A reload replaces the whole text through the same hook.
    run(ed, ":reload!<ret>");
    assert_server_matches(ed, "reload");
    // Every change went out incrementally: no full-text fallback.
    let changes: Vec<Value> = stats(ed)
        .into_iter()
        .filter(|m| m["method"] == "textDocument/didChange")
        .flat_map(|m| m["params"]["contentChanges"].as_array().unwrap().clone())
        .collect();
    assert!(changes.len() >= 12, "{} changes", changes.len());
    assert!(
        changes.iter().all(|c| c.get("range").is_some()),
        "a full-text change was sent"
    );
}

/// The notifications the fake server received, in order.
fn stats(ed: &Editor) -> Vec<Value> {
    let rx = ed
        .lsp
        .raw_request(ed.view().doc, "fake/stats", json!({}))
        .expect("synced");
    let stats = maestro_lsp::client::recv_reply(&rx, WAIT).unwrap().unwrap();
    stats["received"].as_array().unwrap().clone()
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
    // The server's stderr went to the log, which outlives the server.
    run(ed, ":lsp-log<ret>");
    let log = ed.doc().text().to_string();
    assert!(log.contains("(stopped) =="), "{log}");
    assert!(log.contains("fake-lsp: started"), "{log}");
    assert!(log.contains("fake-lsp: crashing on request"), "{log}");
    run(ed, "ga");
    assert_eq!(ed.view().doc, doc);
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

/// Locations can point into any buffer, so an edit to another buffer
/// while a definition request is out also drops the reply.
fn reply_dropped_when_another_buffer_changed() {
    let mut f = open_synced("cross", "// @slow\nfn helper() {}\nhelper();\n");
    let other = f.dir.join("other.rs");
    std::fs::write(&other, "fn other() {}\n").unwrap();
    let ed = &mut f.ed;
    let first = ed.view().doc;
    // Both buffers are open when the request goes out.
    ed.open(&other).unwrap();
    run(ed, "ga3Ggd");
    run(ed, "gaix<esc>ga");
    assert_eq!(ed.view().doc, first);
    assert!(
        wait_until(ed, |ed| status(ed).contains("another buffer changed")),
        "status {:?}",
        ed.status
    );
    assert_ne!(head(ed), 12, "did not jump");
}

fn hover_dropped_when_the_cursor_moved() {
    let mut f = open_synced("hovermove", "// @slow\nfn helper() {}\nhelper();\n");
    let ed = &mut f.ed;
    run(ed, "3G<space>kl");
    assert!(
        wait_until(ed, |ed| status(ed)
            .contains("hover result dropped: the cursor moved")),
        "status {:?}",
        ed.status
    );
    assert!(ed.lsp.hover.is_none());
}

fn save_sends_text_when_asked() {
    let mut f = open_synced_with(
        "savetext",
        "fn main() {}\n",
        fake_config_with(&[fake::SAVE_TEXT_ARG]),
    );
    let ed = &mut f.ed;
    run(ed, "ix<esc>:w<ret>");
    let saved: Vec<Value> = stats(ed)
        .into_iter()
        .filter(|m| m["method"] == "textDocument/didSave")
        .collect();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0]["params"]["text"], "xfn main() {}\n");
}

fn definition_reply_dropped_while_a_prompt_is_open() {
    // A slow reply must not switch files under a half-typed `:` command.
    let mut f = open_synced("prompt", "// @slow\nfn helper() {}\nhelper();\n");
    let ed = &mut f.ed;
    run(ed, "3G");
    let doc = ed.view().doc;
    let before = head(ed);
    run(ed, "gd:%s/helper/other/g");
    assert!(
        wait_until(ed, |ed| status(ed)
            .contains("arrived while you were typing")),
        "status {:?}",
        ed.status
    );
    assert_eq!(ed.view().doc, doc);
    assert_eq!(head(ed), before, "did not jump while the prompt was open");
    assert!(ed.prompt.is_some(), "the user's prompt is still open");
    run(ed, "<esc>");
}

// ----- completion ---------------------------------------------------------

/// Labels in the open popup, in order; empty when none is shown.
fn labels(ed: &Editor) -> Vec<String> {
    completion::visible(ed)
        .map(|m| {
            m.matches
                .iter()
                .map(|x| m.items[x.index].label.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn wait_for_popup(ed: &mut Editor) {
    assert!(
        wait_until(ed, |ed| completion::visible(ed).is_some()),
        "no popup; status {:?}, dropped {:?}",
        ed.status,
        ed.lsp.completion.last_dropped
    );
}

/// The completion requests the fake server answered, with their params.
fn completion_requests(ed: &Editor) -> Vec<Value> {
    let rx = ed
        .lsp
        .raw_request(ed.view().doc, "fake/stats", json!({}))
        .expect("synced");
    let stats = maestro_lsp::client::recv_reply(&rx, WAIT).unwrap().unwrap();
    stats["requests"].as_array().unwrap().clone()
}

fn line(ed: &Editor, n: usize) -> String {
    ed.doc().text().line(n).to_string()
}

fn completion_on_demand_filters_and_accepts() {
    let mut f = open_synced("compl-demand", "fn main() {\n    std::\n}\n");
    let ed = &mut f.ed;
    run(ed, "2GAcol<C-x>");
    wait_for_popup(ed);
    // Fuzzy and case-smart: lowercase `col` also matches `Color`. Equal
    // scores keep the server's sortText order.
    assert_eq!(
        labels(ed),
        ["collections", "column", "Color", "col_label_only"]
    );
    let menu = completion::visible(ed).unwrap();
    let item = &menu.items[menu.matches[0].index];
    assert_eq!(
        (item.kind, item.detail.as_str()),
        ("module", "std::collections")
    );
    let reqs = completion_requests(ed);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["context"], json!({"triggerKind": 1}));
    assert_eq!(reqs[0]["position"], json!({"line": 1, "character": 12}));
    // The list is complete: typing narrows it without asking again.
    run(ed, "l");
    assert_eq!(labels(ed), ["collections", "col_label_only"]);
    run(ed, "<C-n>");
    assert_eq!(completion::visible(ed).unwrap().selected, 1);
    run(ed, "<C-n>");
    assert_eq!(completion::visible(ed).unwrap().selected, 0, "wraps");
    run(ed, "<down><up><C-p>");
    assert_eq!(completion::visible(ed).unwrap().selected, 1);
    run(ed, "<C-p>");
    run(ed, "<tab>");
    assert!(completion::visible(ed).is_none());
    assert_eq!(ed.mode, Mode::Insert, "accepting stays in insert mode");
    assert_eq!(line(ed, 1), "    std::collections\n");
    assert_eq!(head(ed), ed.doc().text().line_to_char(1) + 20);
    run(ed, "::<esc>");
    assert_eq!(line(ed, 1), "    std::collections::\n");
    assert_eq!(completion_requests(ed).len(), 1);
    assert_server_matches(ed, "after completion");
}

fn completion_after_typing_waits_for_the_debounce() {
    let mut f = open_synced("compl-typing", "fn main() {\n    \n}\n");
    let ed = &mut f.ed;
    run(ed, "2GAco");
    let typed = Instant::now();
    // Nothing is sent while the debounce runs.
    assert!(!ed.lsp.completion.is_pending());
    assert!(completion::visible(ed).is_none());
    assert!(
        wait_until(ed, |ed| ed.lsp.completion.is_pending()
            || completion::visible(ed).is_some()),
        "never sent"
    );
    assert!(
        typed.elapsed() >= completion::DEBOUNCE,
        "sent after {:?}",
        typed.elapsed()
    );
    wait_for_popup(ed);
    let reqs = completion_requests(ed);
    assert_eq!(reqs.len(), 1, "one request for the burst of two keys");
    assert_eq!(reqs[0]["context"], json!({"triggerKind": 1}));
    // Nothing matches `cox`: the popup closes.
    run(ed, "x");
    assert!(completion::visible(ed).is_none());
    run(ed, "<esc>");
}

fn completion_on_a_trigger_character() {
    let mut f = open_synced("compl-trigger", "fn main() {\n    v\n}\n");
    let ed = &mut f.ed;
    run(ed, "2GA.");
    // A trigger character asks at once.
    assert!(ed.lsp.completion.is_pending());
    wait_for_popup(ed);
    assert_eq!(labels(ed), ["len", "push"]);
    let reqs = completion_requests(ed);
    assert_eq!(
        reqs.last().unwrap()["context"],
        json!({"triggerKind": 2, "triggerCharacter": "."})
    );
    run(ed, "p");
    assert_eq!(labels(ed), ["push"]);
    // A snippet without $0 leaves the cursor at its end.
    run(ed, "<ret>");
    assert_eq!(line(ed, 1), "    v.push(value)\n");
    assert_eq!(head(ed), ed.doc().text().line_to_char(1) + 17);
    run(ed, "<esc>");
    assert_server_matches(ed, "trigger");
}

fn completion_escape_and_motion_close_the_popup() {
    let mut f = open_synced("compl-close", "fn main() {\n    co\n}\n");
    let ed = &mut f.ed;
    run(ed, "2GA<C-x>");
    wait_for_popup(ed);
    // Escape closes the popup only, as it closes a picker or a prompt.
    run(ed, "<esc>");
    assert!(completion::visible(ed).is_none());
    assert_eq!(ed.mode, Mode::Insert);
    run(ed, "<esc>");
    assert_eq!(ed.mode, Mode::Normal);
    // Moving the cursor closes it too.
    run(ed, "A<C-x>");
    wait_for_popup(ed);
    run(ed, "<left>");
    assert!(completion::visible(ed).is_none());
    assert_eq!(ed.mode, Mode::Insert);
    // Enter with no popup is a newline again.
    run(ed, "<ret><esc>");
    assert_eq!(
        ed.doc().text().to_string(),
        "fn main() {\n    c\n    o\n}\n"
    );
}

fn completion_insert_replace_edit_uses_the_insert_range() {
    let mut f = open_synced("compl-insrep", "fn main() {\n    coxyz\n}\n");
    let ed = &mut f.ed;
    // Insert between `co` and `xyz`.
    run(ed, "2G^lli<C-x>");
    wait_for_popup(ed);
    run(ed, "lu");
    assert_eq!(labels(ed), ["column"]);
    run(ed, "<tab>");
    // The insert range ends at the cursor: `xyz` stays.
    assert_eq!(line(ed, 1), "    columnxyz\n");
    assert_eq!(head(ed), ed.doc().text().line_to_char(1) + 10);
    run(ed, "<esc>");
    assert_server_matches(ed, "insert-replace");
}

fn completion_insert_text_and_label_only_items() {
    let mut f = open_synced("compl-text", "fn main() {\n    Colo\n    col_l\n}\n");
    let ed = &mut f.ed;
    // An uppercase letter makes the filter case-sensitive.
    run(ed, "2GA<C-x>");
    wait_for_popup(ed);
    assert_eq!(labels(ed), ["Color"]);
    run(ed, "<tab><esc>");
    assert_eq!(line(ed, 1), "    Color\n", "insertText replaces the prefix");
    run(ed, "3GA<C-x>");
    wait_for_popup(ed);
    assert_eq!(labels(ed), ["col_label_only"]);
    run(ed, "<tab><esc>");
    assert_eq!(line(ed, 2), "    col_label_only\n", "the label is the text");
    assert_server_matches(ed, "insertText and label");
}

fn completion_additional_edits_apply_in_one_transaction() {
    let text = "fn main() {\n    let m: Hash\n}\n";
    let mut f = open_synced("compl-additional", text);
    let ed = &mut f.ed;
    run(ed, "2GA<C-x>");
    wait_for_popup(ed);
    assert_eq!(labels(ed), ["HashMap"]);
    let version = ed.doc().version().0;
    run(ed, "<tab>");
    assert_eq!(ed.doc().version().0, version + 1, "one transaction");
    assert_eq!(
        ed.doc().text().to_string(),
        "use std::collections::HashMap;\nfn main() {\n    let m: HashMap\n}\n"
    );
    assert_eq!(head(ed), ed.doc().text().line_to_char(2) + 18);
    run(ed, "<esc>");
    assert_server_matches(ed, "additional edits");
    // `.` cannot repeat an edit in two places as typing, so it repeats
    // nothing rather than something else.
    run(ed, ".");
    assert_eq!(ed.doc().version().0, version + 1);
    // The whole insert session is one undo step.
    run(ed, "u");
    assert_eq!(ed.doc().text().to_string(), text);
}

fn completion_snippet_puts_the_cursor_at_the_final_stop() {
    let mut f = open_synced("compl-snippet", "fn main() {\n    conf\n}\n");
    let ed = &mut f.ed;
    run(ed, "2GA<C-x>");
    wait_for_popup(ed);
    assert_eq!(labels(ed), ["configure"]);
    run(ed, "<tab>");
    // `configure(${1:value}, ${2:\$x})$0;`
    assert_eq!(line(ed, 1), "    configure(value, $x);\n");
    run(ed, "X<esc>");
    assert_eq!(line(ed, 1), "    configure(value, $x)X;\n");
}

fn completion_utf16_positions_with_emoji_and_cjk() {
    let before = "    let s = \"😀日本\"; co";
    let text = format!("fn main() {{\n{before}\n}}\n");
    let mut f = open_synced("compl-utf16", &text);
    let ed = &mut f.ed;
    run(ed, "2GA<C-x>");
    wait_for_popup(ed);
    let units: usize = before.chars().map(char::len_utf16).sum();
    assert_eq!(units, before.chars().count() + 1, "the emoji is two units");
    assert_eq!(
        completion_requests(ed)[0]["position"],
        json!({"line": 1, "character": units})
    );
    run(ed, "<tab>");
    assert_eq!(line(ed, 1), format!("{before}llections\n"));
    // Non-ASCII labels, and a CJK prefix inside the server's range.
    run(ed, " caf<C-x>");
    wait_for_popup(ed);
    assert_eq!(labels(ed), ["café_crème"]);
    run(ed, "<tab> 日<C-x>");
    wait_for_popup(ed);
    assert_eq!(labels(ed), ["日本語"]);
    run(ed, "<tab><esc>");
    assert_eq!(
        line(ed, 1),
        format!("{before}llections café_crème 日本語\n")
    );
    assert_server_matches(ed, "utf-16");
}

fn completion_stale_replies_are_dropped() {
    // `@slow` holds every reply for 600 ms.
    let mut f = open_synced("compl-stale", "// @slow\nfn main() {\n    \n}\n");
    let ed = &mut f.ed;
    let doc = ed.view().doc;
    // The text changed while the request was out.
    run(ed, "3GAco<C-x> ");
    let edited = ed.doc().text().to_string();
    assert!(
        wait_until(ed, |ed| ed.lsp.completion.last_dropped.as_deref()
            == Some("the buffer changed")),
        "dropped {:?}",
        ed.lsp.completion.last_dropped
    );
    assert!(completion::visible(ed).is_none());
    assert_eq!(ed.doc().text().to_string(), edited);
    // The cursor moved.
    run(ed, "<C-x><left>");
    assert!(
        wait_until(ed, |ed| ed.lsp.completion.last_dropped.as_deref()
            == Some("the cursor moved")),
        "dropped {:?}",
        ed.lsp.completion.last_dropped
    );
    assert!(completion::visible(ed).is_none());
    // Leaving insert mode cancels the request outright.
    run(ed, "<C-x>");
    assert!(ed.lsp.completion.is_pending());
    run(ed, "<esc>");
    assert!(!ed.lsp.completion.is_pending());
    std::thread::sleep(Duration::from_millis(fake::SLOW_MS + 200));
    ed.poll_background();
    assert!(completion::visible(ed).is_none());
    assert_eq!(ed.view().doc, doc);
    assert_eq!(ed.doc().text().to_string(), edited);
}

fn completion_at_every_cursor_where_the_text_matches() {
    let mut f = open_synced(
        "compl-multi",
        "fn main() {\n    co\n    co\n    xo\n    co\n}\n",
    );
    let ed = &mut f.ed;
    // Cursors at the ends of lines 2 and 3; `C` makes the new one primary.
    run(ed, "2GCA<C-x>");
    assert_eq!(ed.view().selection.len(), 2);
    wait_for_popup(ed);
    run(ed, "<tab>");
    assert_eq!(line(ed, 1), "    collections\n");
    assert_eq!(line(ed, 2), "    collections\n");
    let heads: Vec<usize> = ed.view().selection.iter().map(|r| r.head).collect();
    let text = ed.doc().text().clone();
    assert_eq!(
        heads,
        [text.line_to_char(1) + 15, text.line_to_char(2) + 15]
    );
    run(ed, "<esc>,");
    // A cursor whose text differs from the primary's is left alone.
    run(ed, "4GCA<C-x>");
    assert_eq!(ed.view().selection.primary_index(), 1, "primary on `co`");
    wait_for_popup(ed);
    run(ed, "<tab>");
    assert_eq!(line(ed, 4), "    collections\n");
    assert_eq!(line(ed, 3), "    xo\n", "left alone");
    assert!(
        status(ed).contains("completed at 1 of 2 cursors"),
        "{}",
        status(ed)
    );
    let heads: Vec<usize> = ed.view().selection.iter().map(|r| r.head).collect();
    let text = ed.doc().text().clone();
    assert_eq!(heads, [text.line_to_char(3) + 6, text.line_to_char(4) + 15]);
    run(ed, "<esc>");
    assert_server_matches(ed, "multi-cursor");
}

fn completion_incomplete_list_asks_again() {
    let mut f = open_synced("compl-incomplete", "// @incomplete\nfn main() {\n    \n}\n");
    let ed = &mut f.ed;
    run(ed, "3GAab<C-x>");
    wait_for_popup(ed);
    assert_eq!(labels(ed), ["abEcho"]);
    assert!(completion::visible(ed).unwrap().incomplete);
    run(ed, "c");
    assert!(
        wait_until(ed, |ed| labels(ed) == ["abcEcho"]),
        "labels {:?}",
        labels(ed)
    );
    let reqs = completion_requests(ed);
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[1]["context"], json!({"triggerKind": 3}));
    run(ed, "<tab><esc>");
    assert_eq!(line(ed, 2), "    abcEcho\n");
}

fn completion_dot_repeats_an_accepted_item() {
    let mut f = open_synced("compl-dot", "fn main() {\n    co\n    co\n}\n");
    let ed = &mut f.ed;
    run(ed, "2GA<C-x>");
    wait_for_popup(ed);
    run(ed, "<tab>()<esc>");
    assert_eq!(line(ed, 1), "    collections()\n");
    // `.` replays the session as typed keys, with no server involved.
    run(ed, "j.");
    assert_eq!(line(ed, 2), "    collections()\n");
    assert!(!ed.lsp.completion.is_pending());
    assert_server_matches(ed, "dot repeat");
}

fn completion_in_a_crlf_document_inserts_crlf() {
    let mut f = open_synced("compl-crlf", "fn main() {\r\n    let m: Hash\r\n}\r\n");
    let ed = &mut f.ed;
    run(ed, "2GA<C-x>");
    wait_for_popup(ed);
    run(ed, "<tab>");
    // The import's `\n` arrives as the document's line ending.
    assert_eq!(
        ed.doc().text().to_string(),
        "use std::collections::HashMap;\r\nfn main() {\r\n    let m: HashMap\r\n}\r\n"
    );
    assert_eq!(head(ed), ed.doc().text().line_to_char(2) + 18);
    run(ed, "<esc>");
    assert_server_matches(ed, "crlf");
}

/// Replayed keys act at every cursor, so a completion that skipped a
/// cursor is not repeated by `.`.
fn completion_that_skipped_a_cursor_is_not_repeated() {
    let mut f = open_synced("compl-skipdot", "xo\nco\nxo\nco\n");
    let ed = &mut f.ed;
    run(ed, "CA<C-x>");
    wait_for_popup(ed);
    run(ed, "<tab><esc>");
    assert_eq!(ed.doc().text().to_string(), "xo\ncollections\nxo\nco\n");
    let version = ed.doc().version();
    run(ed, ",jj.");
    assert_eq!(ed.doc().version(), version, "`.` changed nothing");
    assert_eq!(ed.doc().text().to_string(), "xo\ncollections\nxo\nco\n");
}

/// Typing where an item's additional edit goes makes that item stale.
fn completion_item_dropped_when_its_import_spot_is_edited() {
    let mut f = open_synced("compl-importspot", "Hash\n");
    let ed = &mut f.ed;
    run(ed, "A<C-x>");
    wait_for_popup(ed);
    assert_eq!(labels(ed), ["HashMap"]);
    // Clearing the line deletes text at the import's position (0).
    run(ed, "<C-u>H");
    assert!(
        !labels(ed).contains(&"HashMap".to_string()),
        "{:?}",
        labels(ed)
    );
    run(ed, "<tab><esc>");
    assert!(
        !ed.doc().text().to_string().contains("use std"),
        "{:?}",
        ed.doc().text().to_string()
    );
}

/// Moving a cursor other than the primary also closes the popup.
fn completion_closes_when_a_secondary_cursor_moves() {
    use maestro_core::{Range, Selection};
    let mut f = open_synced("compl-secondary", "co\nco\n");
    let ed = &mut f.ed;
    run(ed, "i");
    ed.set_selection(Selection::new(vec![Range::point(0), Range::point(5)], 0));
    run(ed, "<C-x>");
    wait_for_popup(ed);
    // The primary is at the start of the text and cannot move left.
    run(ed, "<left>");
    assert_eq!(head(ed), 0);
    assert!(completion::visible(ed).is_none());
    run(ed, "<tab><esc>");
    assert!(!ed.doc().text().to_string().contains("collections"));
}

/// A debounce started in one view does not fire after a click moved the
/// focus to another.
fn completion_debounce_does_not_follow_a_click_to_another_view() {
    use maestro_editor::MouseKind;
    let mut f = open_synced("compl-click", "fn main() {\n    \n}\n");
    let ed = &mut f.ed;
    run(ed, "<C-w>v2GAco");
    let first = ed.focus;
    // Click into the other split while still in insert mode.
    let other = ed.views.values().find(|v| v.id != first).unwrap().area;
    ed.handle_mouse(MouseKind::Down, other.x + other.width / 2, other.y);
    assert_ne!(ed.focus, first);
    std::thread::sleep(completion::DEBOUNCE * 2);
    ed.poll_background();
    assert!(!ed.lsp.completion.is_pending());
    assert!(completion::visible(ed).is_none());
    run(ed, "<esc>");
}

/// In a view too small to draw the popup, its keys do their usual work.
fn completion_keys_pass_through_when_the_popup_cannot_be_drawn() {
    let mut f = open_synced("compl-small", "fn main() {\n    co\n}\n");
    let ed = &mut f.ed;
    run(ed, "2GA<C-x>");
    wait_for_popup(ed);
    ed.resize(Rect::new(0, 0, 7, 30));
    assert!(completion::visible(ed).is_none());
    run(ed, "<ret><esc>");
    assert!(!ed.doc().text().to_string().contains("collections"));
    assert_eq!(ed.doc().text().len_lines(), 5, "Enter made a new line");
}

// ----- rename, formatting, code actions, signature help ---------------------

/// Everything `fake/stats` returns.
fn fake_stats(ed: &Editor) -> Value {
    let rx = ed
        .lsp
        .raw_request(ed.view().doc, "fake/stats", json!({}))
        .expect("synced");
    maestro_lsp::client::recv_reply(&rx, WAIT).unwrap().unwrap()
}

fn text(ed: &Editor) -> String {
    ed.doc().text().to_string()
}

fn wait_status(ed: &mut Editor, needle: &str) {
    assert!(
        wait_until(ed, |ed| status(ed).contains(needle)),
        "wanted {needle:?} in the status; got {:?}",
        ed.status
    );
}

fn wait_picker(ed: &mut Editor) {
    assert!(
        wait_until(ed, |ed| ed.picker.is_some()),
        "no picker; status {:?}, dropped {:?}",
        ed.status,
        ed.lsp.actions.last_dropped
    );
}

fn rename_prompt_is_prefilled_and_renames_across_files() {
    use maestro_editor::prompt::PromptKind;
    let original = "fn helper() {}\nfn main() {\n    helper();\n}\n";
    let mut f = open_synced("rename", original);
    let util = f.dir.join("util.rs");
    let util_text = "pub fn twice() {\n    helper();\n    helper();\n}\n";
    std::fs::write(&util, util_text).unwrap();
    let ed = &mut f.ed;
    let main_doc = ed.view().doc;
    run(ed, "3G^<space>n");
    assert!(
        wait_until(ed, |ed| ed.prompt.is_some()),
        "no prompt; status {:?}",
        ed.status
    );
    let p = ed.prompt.as_ref().unwrap();
    assert_eq!(p.kind, PromptKind::Rename);
    assert_eq!(p.input, "helper", "prefilled from prepareRename");
    run(ed, "<C-u>assist\n");
    assert!(
        wait_until(ed, |ed| text(ed).contains("assist")),
        "status {:?}",
        ed.status
    );
    assert_eq!(text(ed), "fn assist() {}\nfn main() {\n    assist();\n}\n");
    // The file that was not open is a buffer now, edited but not saved,
    // and the view did not move to it.
    let util_id = ed
        .find_buffer_by_path(&util)
        .expect("util.rs opened as a buffer");
    let util_doc = &ed.buffers[&util_id].doc;
    assert_eq!(
        util_doc.text().to_string(),
        "pub fn twice() {\n    assist();\n    assist();\n}\n"
    );
    assert!(util_doc.is_modified());
    assert_eq!(std::fs::read_to_string(&util).unwrap(), util_text);
    assert_eq!(ed.view().doc, main_doc);
    let st = status(ed);
    assert!(
        st.contains("renamed to assist: 4 edits in 2 files changed, not saved")
            && st.contains("1 file opened in the background"),
        "{st}"
    );
    // The server now has util.rs open with the edited text.
    assert!(wait_until(ed, |ed| ed.lsp.is_synced(util_id)));
    // One undo step per buffer.
    run(ed, "u");
    assert_eq!(text(ed), original);
    assert_server_matches(ed, "undo after rename");
}

fn rename_without_prepare_and_as_a_typed_command() {
    let config = fake_config_with(&[fake::NO_PREPARE_RENAME_ARG]);
    let mut f = open_synced_with("rename-noprep", "fn helper() {}\nhelper();\n", config);
    let ed = &mut f.ed;
    run(ed, "2G<space>n");
    // No request: the prompt opens at once with the word at the cursor.
    let p = ed.prompt.as_ref().expect("prompt");
    assert_eq!(p.input, "helper");
    run(ed, "<esc>");
    assert!(ed.prompt.is_none());
    run(ed, ":rename other\n");
    assert!(
        wait_until(ed, |ed| text(ed) == "fn other() {}\nother();\n"),
        "status {:?}",
        ed.status
    );
    assert!(
        status(ed).contains("1 file changed, not saved"),
        "{:?}",
        ed.status
    );
}

/// A version mismatch, overlapping edits (in a file other than the open
/// one), a range past the end and a file operation each refuse the whole
/// edit: no buffer changes and no file is opened.
fn rename_refuses_bad_edits_and_changes_nothing() {
    for (name, needle) in [
        ("badversion", "changed since the server made the edit"),
        ("overlap", "two edits overlap at line 1"),
        ("outofbounds", "past the end of the text"),
        (
            "createfile",
            "create a file, which Maestro does not support",
        ),
    ] {
        let original = format!("// @{name}\nfn helper() {{}}\nhelper();\n");
        let mut f = open_synced(&format!("rename-{name}"), &original);
        let other = f.dir.join("other.rs");
        std::fs::write(&other, "fn x() { helper(); }\n").unwrap();
        let ed = &mut f.ed;
        run(ed, "3G:rename renamed\n");
        wait_status(ed, needle);
        assert!(
            status(ed).contains("nothing was changed"),
            "{name}: {:?}",
            ed.status
        );
        assert_eq!(text(ed), original, "{name}");
        assert!(!ed.doc().is_modified(), "{name}");
        assert!(ed.find_buffer_by_path(&other).is_none(), "{name}");
        assert_eq!(ed.buffers.len(), 1, "{name}");
    }
}

fn rename_reports_server_errors_and_unrenamable_spots() {
    let original = "fn builtin() {}\nfn helper() {}\n\n";
    let mut f = open_synced("rename-errors", original);
    let ed = &mut f.ed;
    run(ed, "gg:rename error\n");
    wait_status(ed, "rename: the fake rename failed");
    // prepareRename answers null on an empty line ...
    run(ed, "3G<space>n");
    wait_status(ed, "nothing here can be renamed");
    assert!(ed.prompt.is_none());
    // ... and an error on `builtin`.
    run(ed, "ggfb<space>n");
    wait_status(ed, "cannot rename a builtin");
    assert!(ed.prompt.is_none());
    assert_eq!(text(ed), original);
}

fn rename_reply_dropped_after_typing() {
    let mut f = open_synced("rename-stale", "// @slow\nfn helper() {}\nhelper();\n");
    let ed = &mut f.ed;
    run(ed, "3G:rename other\n");
    assert!(ed.lsp.actions.is_pending());
    run(ed, "ix<esc>");
    let edited = text(ed);
    assert!(
        wait_until(ed, |ed| ed.lsp.actions.last_dropped.as_deref()
            == Some("a buffer changed while waiting")),
        "dropped {:?}",
        ed.lsp.actions.last_dropped
    );
    assert_eq!(text(ed), edited);
    assert!(!text(ed).contains("other"));
    assert!(
        status(ed).contains("rename result dropped"),
        "{:?}",
        ed.status
    );
}

fn rename_accepts_the_changes_form() {
    let mut f = open_synced("rename-changes", "// @changes\nfn helper() {}\nhelper();\n");
    let ed = &mut f.ed;
    run(ed, "3G:rename other\n");
    assert!(
        wait_until(ed, |ed| text(ed)
            == "// @changes\nfn other() {}\nother();\n"),
        "status {:?}",
        ed.status
    );
}

fn format_is_one_undo_step_and_keeps_the_cursor() {
    let original = "fn main() {   \n    let x = 1;  \n    x;\t\n}\n";
    let mut f = open_synced("format", original);
    let ed = &mut f.ed;
    run(ed, "3G^");
    run(ed, ":format\n");
    let formatted = "fn main() {\n    let x = 1;\n    x;\n}\n";
    assert!(
        wait_until(ed, |ed| text(ed) == formatted),
        "status {:?}",
        ed.status
    );
    let t = ed.doc().text().clone();
    assert_eq!(head(ed), t.line_to_char(2) + 4, "still on the `x`");
    assert!(
        status(ed).contains("formatted: 3 edits, not saved"),
        "{:?}",
        ed.status
    );
    assert_server_matches(ed, "format");
    run(ed, "u");
    assert_eq!(text(ed), original, "one undo step");
    run(ed, "U<space>=");
    wait_status(ed, "already formatted");
    assert_eq!(text(ed), formatted);
}

/// A server that answers with one edit replacing the whole text: only the
/// lines that differ change, so the cursor keeps its place.
fn format_whole_text_reply_keeps_the_cursor_line() {
    let original = "// @formatwhole\nfn main() {  \n    let x = 1;\n    x;  \n}\n";
    let mut f = open_synced("format-whole", original);
    let ed = &mut f.ed;
    run(ed, "3G^4l");
    let before = head(ed);
    assert_eq!(ed.doc().text().char(before), 'x');
    run(ed, ":format\n");
    let formatted = "// @formatwhole\nfn main() {\n    let x = 1;\n    x;\n}\n";
    assert!(
        wait_until(ed, |ed| text(ed) == formatted),
        "status {:?}",
        ed.status
    );
    assert_eq!(ed.doc().text().char(head(ed)), 'x', "head {}", head(ed));
    assert_eq!(ed.doc().text().char_to_line(head(ed)), 2);
    run(ed, "u");
    assert_eq!(text(ed), original);
}

fn format_selection_formats_only_the_selected_lines() {
    let mut f = open_synced("format-range", "a  \nb  \nc  \n");
    let ed = &mut f.ed;
    run(ed, "2Gx:format-selection\n");
    assert!(
        wait_until(ed, |ed| text(ed) == "a  \nb\nc  \n"),
        "text {:?}, status {:?}",
        text(ed),
        ed.status
    );
}

fn format_errors_and_stale_replies_change_nothing() {
    let original = "// @formaterror\nx  \n";
    let mut f = open_synced("format-error", original);
    let ed = &mut f.ed;
    run(ed, ":format\n");
    wait_status(ed, "format: the fake formatter failed");
    assert_eq!(text(ed), original);
    drop(f);

    let mut f = open_synced("format-stale", "// @slow\nx  \n");
    let ed = &mut f.ed;
    run(ed, ":format\n");
    run(ed, "2GAy<esc>");
    let edited = text(ed);
    assert!(
        wait_until(ed, |ed| ed.lsp.actions.last_dropped.as_deref()
            == Some("a buffer changed while waiting")),
        "dropped {:?}",
        ed.lsp.actions.last_dropped
    );
    assert_eq!(text(ed), edited);
    assert!(text(ed).contains("x  y"));
}

fn picker_labels(ed: &Editor) -> Vec<(String, String)> {
    ed.picker
        .as_ref()
        .unwrap()
        .items()
        .iter()
        .map(|i| (i.label.clone(), i.detail.clone()))
        .collect()
}

fn code_action_quick_fix_sends_diagnostics_and_runs_its_command() {
    let original = "fn main() {\n    let x = ERROR;\n}\n";
    let mut f = open_synced("action-fix", original);
    let ed = &mut f.ed;
    let doc = ed.view().doc;
    assert!(wait_until(ed, |ed| ed.lsp.diagnostics(doc).len() == 1));
    // Move the diagnostic: the request must send its current range.
    run(ed, "ggO// moved<esc>");
    assert!(wait_until(ed, |ed| ed.lsp.diagnostics(doc).len() == 1));
    run(ed, "3G^fE<space>c");
    wait_picker(ed);
    let items = picker_labels(ed);
    assert_eq!(
        items[0],
        ("Replace ERROR with OK".into(), "quickfix, preferred".into())
    );
    assert!(items.contains(&(
        "Unavailable action".into(),
        "refactor, unavailable: not available here".into()
    )));
    assert_eq!(items.len(), 5, "{items:?}");
    run(ed, "<ret>");
    assert!(
        wait_until(ed, |ed| text(ed).contains("let x = OK;")),
        "status {:?}",
        ed.status
    );
    // Its command ran after the edit: the server saw the edited text when
    // the command arrived.
    assert!(wait_until(ed, |ed| {
        fake_stats(ed)["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["command"] == "fake.record" && c["arguments"] == json!(["quickfix"]))
    }));
    let commands = fake_stats(ed)["commands"].clone();
    let record = commands
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["command"] == "fake.record")
        .unwrap();
    assert!(
        record["openTexts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t.as_str().unwrap().contains("let x = OK;")),
        "{record}"
    );
    run(ed, "u");
    assert_eq!(text(ed), format!("// moved\n{original}"));
}

fn code_action_resolve_and_server_edit_request() {
    let mut f = open_synced("action-resolve", "fn main() {\n    hello();\n}\n");
    let ed = &mut f.ed;
    run(ed, "2G^<space>c");
    wait_picker(ed);
    // No edit in the list: it comes from codeAction/resolve.
    run(ed, "Uppercase<ret>");
    assert!(
        wait_until(ed, |ed| text(ed).contains("    HELLO();")),
        "status {:?}",
        ed.status
    );
    // A bare command; the server answers it with workspace/applyEdit.
    run(ed, "<space>c");
    wait_picker(ed);
    run(ed, "header<ret>");
    assert!(
        wait_until(ed, |ed| text(ed).starts_with("// header\nfn main()")),
        "status {:?}",
        ed.status
    );
    assert!(
        status(ed).contains("fake edit: 1 file changed"),
        "{:?}",
        ed.status
    );
    assert!(wait_until(ed, |ed| {
        fake_stats(ed)["responses"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["result"] == json!({"applied": true}))
    }));
    assert_server_matches(ed, "server edit");
}

fn code_action_failures_and_refused_server_edit() {
    let original = "// @badheader\nfn main() {}\n";
    let mut f = open_synced("action-fail", original);
    let ed = &mut f.ed;
    run(ed, "<space>c");
    wait_picker(ed);
    run(ed, "failing<ret>");
    wait_status(ed, "command: the fake command failed");
    run(ed, "<space>c");
    wait_picker(ed);
    run(ed, "Unavailable<ret>");
    assert!(
        status(ed).contains("unavailable: not available here"),
        "{:?}",
        ed.status
    );
    // The server's edit points past the end: refused, and the server is
    // told why.
    run(ed, "<space>c");
    wait_picker(ed);
    run(ed, "header<ret>");
    assert!(
        wait_until(ed, |ed| {
            fake_stats(ed)["responses"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| {
                    r["result"]["applied"] == json!(false)
                        && r["result"]["failureReason"]
                            .as_str()
                            .is_some_and(|s| s.contains("past the end"))
                })
        }),
        "status {:?}",
        ed.status
    );
    assert!(
        status(ed).contains("refused, nothing was changed"),
        "{:?}",
        ed.status
    );
    assert_eq!(text(ed), original);
}

fn code_action_stale_list_and_stale_choice_change_nothing() {
    let mut f = open_synced("action-stale", "// @slow\nfn main() {}\n");
    let ed = &mut f.ed;
    run(ed, "2G<space>c");
    run(ed, "ix<esc>");
    assert!(
        wait_until(ed, |ed| ed.lsp.actions.last_dropped.as_deref()
            == Some("a buffer changed while waiting")),
        "dropped {:?}",
        ed.lsp.actions.last_dropped
    );
    assert!(ed.picker.is_none());
    drop(f);

    // The list is shown, then the buffer changes behind the picker (an
    // automation client, say): choosing an action is refused.
    let mut f = open_synced("action-stale-choice", "fn main() {\n    hello();\n}\n");
    let ed = &mut f.ed;
    run(ed, "2G^<space>c");
    wait_picker(ed);
    let doc = ed.view().doc;
    let version = ed.doc().version();
    let cs = maestro_core::ChangeSet::from_changes(
        ed.doc().text().len_chars(),
        [maestro_core::Change::insert(0, "// x\n")],
    )
    .unwrap();
    maestro_editor::proposals::apply_versioned(ed, doc, version, &cs).unwrap();
    let edited = text(ed);
    run(ed, "Uppercase<ret>");
    assert!(
        status(ed).contains("a buffer changed since the list was made"),
        "{:?}",
        ed.status
    );
    std::thread::sleep(Duration::from_millis(100));
    ed.poll_background();
    assert_eq!(text(ed), edited);
}

/// A file that was not open when the rename went out is opened and edited
/// before the answer arrives. The answer's unversioned edits for it were
/// computed from the file on disk, so it is dropped.
fn rename_reply_dropped_when_a_file_is_opened_while_waiting() {
    let mut f = open_synced("rename-opened", "// @slow\nfn helper() {}\nhelper();\n");
    let other = f.dir.join("other.rs");
    std::fs::write(&other, "fn x() { helper(); }\n").unwrap();
    let ed = &mut f.ed;
    run(ed, "3G:rename renamed\n");
    assert!(ed.lsp.actions.is_pending());
    run(ed, &format!(":open {}\nixx <esc>", other.display()));
    assert_eq!(text(ed), "xx fn x() { helper(); }\n");
    assert!(
        wait_until(ed, |ed| ed.lsp.actions.last_dropped.as_deref()
            == Some("a buffer changed while waiting")),
        "dropped {:?}, status {:?}",
        ed.lsp.actions.last_dropped,
        ed.status
    );
    assert_eq!(text(ed), "xx fn x() { helper(); }\n");
}

/// The cursor moves while `prepareRename` is out: no prompt opens, so the
/// rename cannot go to the symbol the cursor moved to.
fn rename_prepare_dropped_when_the_cursor_moves() {
    let original = "// @slow\nfn foo() {}\nfn bar() {}\n";
    let mut f = open_synced("rename-moved", original);
    let ed = &mut f.ed;
    run(ed, "2Gw<space>n");
    assert!(ed.lsp.actions.is_pending());
    run(ed, "j");
    assert!(
        wait_until(ed, |ed| ed.lsp.actions.last_dropped.as_deref()
            == Some("the cursor moved")),
        "dropped {:?}",
        ed.lsp.actions.last_dropped
    );
    assert!(ed.prompt.is_none());
    assert_eq!(text(ed), original);
}

/// A command's edit comes back as `workspace/applyEdit` after the user
/// typed: it was computed from the old text and is refused.
fn command_edit_refused_after_typing() {
    let original = "// @slow\nfn main() {}\n";
    let mut f = open_synced("command-stale", original);
    let ed = &mut f.ed;
    run(ed, "2G<space>c");
    wait_picker(ed);
    run(ed, "header<ret>");
    run(ed, "ix<esc>");
    let edited = text(ed);
    assert!(
        wait_until(ed, |ed| {
            fake_stats(ed)["responses"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| {
                    r["result"]["applied"] == json!(false)
                        && r["result"]["failureReason"]
                            .as_str()
                            .is_some_and(|s| s.contains("changed since the command was sent"))
                })
        }),
        "status {:?}",
        ed.status
    );
    assert_eq!(text(ed), edited);
    assert!(!text(ed).contains("// header"));
}

/// `.` after a code action does not replay the picker keys (which would
/// run as normal-mode commands before the next picker arrives), and a
/// macro being recorded stops at the picker.
fn code_action_is_not_repeated_or_recorded() {
    let mut f = open_synced("action-repeat", "fn main() {\n    let x = ERROR;\n}\n");
    let ed = &mut f.ed;
    let doc = ed.view().doc;
    assert!(wait_until(ed, |ed| ed.lsp.diagnostics(doc).len() == 1));
    run(ed, "2G^fE<space>c");
    wait_picker(ed);
    run(ed, "OK<ret>");
    assert!(wait_until(ed, |ed| text(ed).contains("let x = OK;")));
    let after = text(ed);
    run(ed, ".");
    std::thread::sleep(Duration::from_millis(200));
    ed.poll_background();
    assert_eq!(ed.mode, Mode::Normal);
    assert_eq!(text(ed), after, "`.` replayed nothing");
    run(ed, "<esc>");
    ed.picker = None;
    // Recording a macro stops when the picker is asked for.
    let before = text(ed);
    run(ed, "Q");
    assert!(ed.recording.is_some());
    run(ed, "<space>c");
    assert!(ed.recording.is_none());
    assert!(
        status(ed).contains("recording into register @ stopped"),
        "{:?}",
        ed.status
    );
    assert_eq!(text(ed), before);
    wait_picker(ed);
    run(ed, "<esc>");
}

fn signature_active(ed: &Editor) -> Option<Option<(usize, usize)>> {
    let p = lsp::signature::visible(ed)?;
    Some(p.help.signatures[p.help.active].active)
}

fn signature_help_follows_the_parameter_and_closes() {
    let mut f = open_synced("sig", "fn add(a: u32, b: u32) -> u32 { a + b }\n\n");
    let ed = &mut f.ed;
    run(ed, "2Giadd(");
    assert!(
        wait_until(ed, |ed| signature_active(ed) == Some(Some((7, 13)))),
        "popup {:?}, dropped {:?}",
        ed.lsp.signature.popup,
        ed.lsp.signature.last_dropped
    );
    let p = lsp::signature::visible(ed).unwrap();
    assert_eq!(p.help.signatures[0].label, "fn add(a: u32, b: u32)");
    run(ed, "1, ");
    assert!(
        wait_until(ed, |ed| signature_active(ed) == Some(Some((15, 21)))),
        "popup {:?}",
        ed.lsp.signature.popup
    );
    // Past the closing parenthesis the server answers null: closed.
    run(ed, "2)");
    assert!(
        wait_until(ed, |ed| lsp::signature::visible(ed).is_none()
            && !ed.lsp.signature.is_pending()),
        "popup {:?}",
        ed.lsp.signature.popup
    );
    run(ed, "<esc>");
}

fn signature_help_on_demand_and_leaving_insert_mode() {
    let mut f = open_synced("sig-demand", "fn add(a: u32, b: u32) {}\nadd(1, 2);\n");
    let ed = &mut f.ed;
    run(ed, "2G7li<C-k>");
    assert!(
        wait_until(ed, |ed| signature_active(ed) == Some(Some((15, 21)))),
        "popup {:?}, status {:?}",
        ed.lsp.signature.popup,
        ed.status
    );
    run(ed, "<esc>");
    assert!(
        ed.lsp.signature.popup.is_none(),
        "closed on leaving insert mode"
    );
    // Outside a call, asking says so.
    run(ed, "ggi<C-k>");
    wait_status(ed, "no signature help here");
    assert!(lsp::signature::visible(ed).is_none());
    run(ed, "<esc>");
}

fn signature_help_stale_replies_are_not_shown() {
    use maestro_editor::MouseKind;
    let mut f = open_synced("sig-stale", "// @slow\nfn add(a: u32, b: u32) {}\n\n");
    let ed = &mut f.ed;
    // Leaving insert mode cancels the request.
    run(ed, "3Giadd(<esc>");
    assert!(!ed.lsp.signature.is_pending());
    std::thread::sleep(Duration::from_millis(fake::SLOW_MS + 200));
    ed.poll_background();
    assert!(ed.lsp.signature.popup.is_none());
    // Typing on while a request is out asks again; only the answer for
    // the latest text is shown.
    run(ed, "A1,x");
    assert!(
        wait_until(ed, |ed| signature_active(ed) == Some(Some((15, 21)))),
        "popup {:?}",
        ed.lsp.signature.popup
    );
    // A click moves the cursor without a key: the help on screen no longer
    // fits and is hidden at once, and the reply in flight is dropped.
    run(ed, "<C-k>");
    assert!(lsp::signature::visible(ed).is_some());
    let area = ed.view().area;
    ed.handle_mouse(MouseKind::Down, area.x + 10, area.y);
    assert!(
        lsp::signature::visible(ed).is_none(),
        "hidden after the click"
    );
    assert!(
        wait_until(ed, |ed| ed.lsp.signature.last_dropped.as_deref()
            == Some("the cursor moved")),
        "dropped {:?}",
        ed.lsp.signature.last_dropped
    );
    run(ed, "<esc>");
}
