use std::path::Path;
use std::time::Duration;

use maestro_core::{Change, ChangeSet, Rope};

use super::*;

fn scope_at(hl: &[Highlight], pos: usize) -> Option<&'static str> {
    hl.iter()
        .find(|(a, b, _)| *a <= pos && pos < *b)
        .map(|(_, _, s)| *s)
}

/// The span covering exactly `needle` (first occurrence), as chars.
fn span_of(text: &str, needle: &str) -> (usize, usize) {
    let byte = text.find(needle).expect("needle present");
    let start = text[..byte].chars().count();
    (start, start + needle.chars().count())
}

fn whole(syntax: &Syntax, text: &Rope) -> Vec<Highlight> {
    syntax.highlights(text, 0..text.len_chars())
}

#[test]
fn detects_languages_from_paths_and_shebangs() {
    let cases = [
        ("src/main.rs", Some(Language::Rust)),
        ("a/b.py", Some(Language::Python)),
        ("stub.pyi", Some(Language::Python)),
        ("Cargo.toml", Some(Language::Toml)),
        ("Cargo.lock", Some(Language::Toml)),
        ("package.json", Some(Language::Json)),
        ("README.md", Some(Language::Markdown)),
        ("notes.MARKDOWN", Some(Language::Markdown)),
        ("app.mjs", Some(Language::JavaScript)),
        ("view.jsx", Some(Language::JavaScript)),
        ("lib.ts", Some(Language::TypeScript)),
        ("view.tsx", Some(Language::Tsx)),
        ("x.c", Some(Language::C)),
        ("x.h", Some(Language::C)),
        ("Makefile", None),
        ("photo.png", None),
        ("noext", None),
    ];
    for (path, want) in cases {
        assert_eq!(Language::from_path(Path::new(path)), want, "{path}");
    }
    assert_eq!(
        Language::from_shebang("#!/usr/bin/env python3"),
        Some(Language::Python)
    );
    assert_eq!(
        Language::from_shebang("#!/usr/bin/python3.12"),
        Some(Language::Python)
    );
    assert_eq!(
        Language::from_shebang("#!/usr/bin/env -S node --flag"),
        Some(Language::JavaScript)
    );
    assert_eq!(Language::from_shebang("#!/bin/sh"), None);
    assert_eq!(Language::from_shebang("print('no shebang')"), None);
    // The path wins over the shebang.
    assert_eq!(
        Language::detect(Some(Path::new("x.rs")), "#!/usr/bin/env python"),
        Some(Language::Rust)
    );
    assert_eq!(
        Language::detect(Some(Path::new("script")), "#!/usr/bin/env python"),
        Some(Language::Python)
    );
    assert_eq!(Language::detect(None, ""), None);
}

/// Every grammar loads with the linked tree-sitter (ABI check), parses a
/// sample without errors, compiles its highlight query and yields
/// highlights.
#[test]
fn every_language_parses_and_highlights() {
    let samples = [
        (Language::Rust, "fn main() { let x = 1; }\n"),
        (Language::Python, "def f(x):\n    return 'a' + x\n"),
        (Language::Toml, "[package]\nname = \"maestro\"\n"),
        (Language::Json, "{\"a\": [1, true, null]}\n"),
        (
            Language::Markdown,
            "# Title\n\n- item\n\n```rust\nfn x() {}\n```\n",
        ),
        (Language::JavaScript, "function f(a) { return a + 1; }\n"),
        (
            Language::TypeScript,
            "function f(a: number): string { return ''; }\n",
        ),
        (Language::Tsx, "const x = <div className=\"a\">hi</div>;\n"),
        (Language::C, "int main(void) { return 0; }\n"),
    ];
    for (lang, src) in samples {
        let text = Rope::from_str(src);
        let syntax = Syntax::parse(lang, &text).unwrap_or_else(|| panic!("{lang:?} loads"));
        assert!(
            !syntax.tree().root_node().has_error(),
            "{lang:?}: {}",
            syntax.tree().root_node().to_sexp()
        );
        assert!(
            lang.highlights().query.is_some(),
            "{lang:?} highlight query compiles"
        );
        assert!(!whole(&syntax, &text).is_empty(), "{lang:?} highlights");
    }
}

#[test]
fn rust_keywords_functions_and_strings_land_on_the_right_chars() {
    let src = "fn main() {\n    let s = \"hi\";\n    helper(s);\n}\n";
    let text = Rope::from_str(src);
    let syntax = Syntax::parse(Language::Rust, &text).unwrap();
    let hl = whole(&syntax, &text);

    assert!(hl.contains(&(0, 2, "keyword")), "{hl:?}");
    let (a, b) = span_of(src, "main");
    assert!(hl.contains(&(a, b, "function")), "{hl:?}");
    let (a, b) = span_of(src, "let");
    assert!(hl.contains(&(a, b, "keyword")), "{hl:?}");
    let (a, b) = span_of(src, "\"hi\"");
    assert!(hl.contains(&(a, b, "string")), "{hl:?}");
    let (a, b) = span_of(src, "helper");
    assert!(hl.contains(&(a, b, "function")), "{hl:?}");

    // Sorted and non-overlapping.
    for w in hl.windows(2) {
        assert!(w[0].1 <= w[1].0, "{w:?}");
    }
}

#[test]
fn highlights_are_clipped_to_the_requested_range() {
    let src = "fn one() {}\nfn two() {}\n";
    let text = Rope::from_str(src);
    let syntax = Syntax::parse(Language::Rust, &text).unwrap();
    let second = text.line_to_char(1);
    let hl = syntax.highlights(&text, second + 1..text.len_chars());
    assert!(hl.iter().all(|(a, _, _)| *a > second), "{hl:?}");
    // `fn` on line two is cut to its second char.
    assert_eq!(scope_at(&hl, second + 1), Some("keyword"));
    let (a, b) = span_of(src, "two");
    assert!(hl.contains(&(a, b, "function")), "{hl:?}");
}

#[test]
fn highlights_count_chars_not_bytes_after_multibyte_text() {
    let src = "// é漢字\nfn main() {}\n";
    let text = Rope::from_str(src);
    let syntax = Syntax::parse(Language::Rust, &text).unwrap();
    let hl = whole(&syntax, &text);
    let (a, b) = span_of(src, "main");
    assert!(hl.contains(&(a, b, "function")), "{hl:?}");
    let comment_end = span_of(src, "\n").0;
    assert_eq!(scope_at(&hl, 0), Some("comment"));
    assert_eq!(scope_at(&hl, comment_end - 1), Some("comment"));
}

#[test]
fn markdown_scopes_map_onto_theme_names() {
    let src = "# Title\n\n```\ncode\n```\n";
    let text = Rope::from_str(src);
    let syntax = Syntax::parse(Language::Markdown, &text).unwrap();
    let hl = whole(&syntax, &text);
    let (a, _) = span_of(src, "Title");
    assert_eq!(scope_at(&hl, a), Some("markup.heading"), "{hl:?}");
    let (a, _) = span_of(src, "code");
    assert_eq!(scope_at(&hl, a), Some("markup.raw"), "{hl:?}");
}

/// A tiny deterministic generator, so the edit sequence is reproducible.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

fn assert_same_as_fresh(syntax: &Syntax, text: &Rope, step: &str) {
    let fresh = Syntax::parse(syntax.language(), text).unwrap();
    assert_eq!(
        syntax.tree().root_node().to_sexp(),
        fresh.tree().root_node().to_sexp(),
        "tree differs after {step}"
    );
    assert_eq!(
        syntax.tree().root_node().byte_range(),
        fresh.tree().root_node().byte_range(),
        "root range differs after {step}"
    );
    assert_eq!(
        whole(syntax, text),
        whole(&fresh, text),
        "highlights differ after {step}"
    );
}

#[test]
fn incremental_edits_match_a_fresh_parse() {
    let src = "fn main() {\n    let name = \"é\";\n    println!(\"{name} 漢字\");\n}\n\
               struct Point { x: i32, y: i32 }\n";
    let mut text = Rope::from_str(src);
    let mut syntax = Syntax::parse(Language::Rust, &text).unwrap();
    let inserts = [
        "é", "漢", "x", " ", "\n", "fn ", "\"", "{", "}", "// c\n", "😀",
    ];
    let mut rng = Lcg(7);
    for step in 0..300 {
        let len = text.len_chars();
        let from = rng.next(len + 1);
        let to = (from + rng.next(4)).min(len);
        let change = match rng.next(3) {
            0 => Change::insert(from, inserts[rng.next(inserts.len())]),
            1 => Change::delete(from, to),
            _ => Change::replace(from, to, inserts[rng.next(inserts.len())]),
        };
        let cs = ChangeSet::from_changes(len, [change.clone()]).unwrap();
        let old = text.clone();
        cs.apply(&mut text).unwrap();
        syntax.update(&old, std::slice::from_ref(&cs), &text);
        assert_same_as_fresh(&syntax, &text, &format!("step {step} {change:?}"));
    }
}

#[test]
fn multi_cursor_change_sets_and_several_sets_at_once() {
    let src = "fn a() {}\nfn b() {}\nfn c() {}\n";
    let mut text = Rope::from_str(src);
    let mut syntax = Syntax::parse(Language::Rust, &text).unwrap();
    let old = text.clone();
    // One set with three insertions (three cursors), then a second set that
    // replaces across lines, applied as one batch like an undo step.
    let first = ChangeSet::from_changes(
        text.len_chars(),
        [
            Change::insert(3, "é"),
            Change::insert(13, "漢x"),
            Change::insert(23, "pub "),
        ],
    )
    .unwrap();
    first.apply(&mut text).unwrap();
    let second =
        ChangeSet::from_changes(text.len_chars(), [Change::replace(5, 20, "() { 1 }\nfn d")])
            .unwrap();
    second.apply(&mut text).unwrap();
    syntax.update(&old, &[first, second], &text);
    assert_same_as_fresh(&syntax, &text, "two sets");
}

#[test]
fn undo_path_restores_the_original_tree() {
    let src = "fn main() {\n    let s = \"é漢\";\n}\n";
    let original = Rope::from_str(src);
    let mut text = original.clone();
    let mut syntax = Syntax::parse(Language::Rust, &text).unwrap();
    let before = whole(&syntax, &text);

    // An edit made of a delete plus an insert, then its inverse, which
    // (as the history stores it) puts the insert before the delete.
    let cs = ChangeSet::from_changes(
        text.len_chars(),
        [
            Change::replace(16, 22, "漢字 + 1"),
            Change::insert(text.len_chars(), "// end\n"),
        ],
    )
    .unwrap();
    let inverse = cs.invert(&text);
    let old = text.clone();
    cs.apply(&mut text).unwrap();
    syntax.update(&old, std::slice::from_ref(&cs), &text);
    assert_same_as_fresh(&syntax, &text, "edit");

    let old = text.clone();
    inverse.apply(&mut text).unwrap();
    syntax.update(&old, std::slice::from_ref(&inverse), &text);
    assert_eq!(text, original);
    assert_same_as_fresh(&syntax, &text, "undo");
    assert_eq!(whole(&syntax, &text), before);
}

#[test]
fn edits_use_byte_offsets_and_byte_columns() {
    let text = Rope::from_str("é\n漢x");
    // Insert "y" after "漢" (char 3, byte 2 + 1 + 3 = 6).
    let cs = ChangeSet::from_changes(text.len_chars(), [Change::insert(3, "y")]).unwrap();
    let edits = input_edits(&text, &[cs]);
    assert_eq!(edits.len(), 1);
    let e = edits[0];
    assert_eq!((e.start_byte, e.old_end_byte, e.new_end_byte), (6, 6, 7));
    assert_eq!(e.start_position, Point { row: 1, column: 3 });
    assert_eq!(e.new_end_position, Point { row: 1, column: 4 });
}

#[test]
fn expand_shrink_and_siblings_follow_the_tree() {
    let src = "fn main() {\n    let total = add(1, 2);\n    other();\n}\n";
    let text = Rope::from_str(src);
    let syntax = Syntax::parse(Language::Rust, &text).unwrap();
    let range = |s: &str| {
        let (a, b) = span_of(src, s);
        a..b
    };

    // From one char inside `add` to the identifier, the call, the statement.
    let start = range("add").start;
    let ident = syntax.expand(&text, start, start + 1).unwrap();
    assert_eq!(ident, range("add"));
    let call = syntax.expand(&text, ident.start, ident.end).unwrap();
    assert_eq!(call, range("add(1, 2)"));
    let stmt = syntax.expand(&text, call.start, call.end).unwrap();
    assert_eq!(stmt, range("let total = add(1, 2);"));

    // Shrinking without history picks the first smaller child.
    let shrunk = syntax.shrink(&text, call.start, call.end).unwrap();
    assert_eq!(shrunk, range("add"));

    // Siblings: the statement's next named sibling is the next statement.
    let next = syntax.sibling(&text, stmt.start, stmt.end, true).unwrap();
    assert_eq!(next, range("other();"));
    let prev = syntax.sibling(&text, next.start, next.end, false).unwrap();
    assert_eq!(prev, stmt);
    // Inside the argument list, `1` goes to `2`.
    let one = range("1, 2").start;
    let two = syntax.sibling(&text, one, one + 1, true).unwrap();
    assert_eq!(two, one + 3..one + 4);

    // The whole document cannot grow.
    assert_eq!(syntax.expand(&text, 0, text.len_chars()), None);
}

fn many_items(n: usize) -> String {
    (0..n)
        .map(|i| format!("fn item_{i}(x: u32) -> u32 {{ let y = x + {i}; y * 2 }}\n"))
        .collect()
}

#[test]
fn budgets_stop_parsing_and_a_fork_finishes_the_job() {
    let mut text = Rope::from_str(&many_items(5000));
    assert!(Syntax::parse_within(Language::Rust, &text, Duration::ZERO).is_none());
    let mut syntax =
        Syntax::parse_within(Language::Rust, &text, Duration::from_secs(60)).expect("parses");

    let old = text.clone();
    let cs = ChangeSet::from_changes(text.len_chars(), [Change::insert(10, "é漢 ")]).unwrap();
    cs.apply(&mut text).unwrap();
    syntax.apply_edits(&input_edits(&old, &[cs]));
    // Out of time: the tree stays the edited old tree.
    assert!(!syntax.reparse_within(&text, Duration::ZERO));
    assert_eq!(
        syntax.tree().root_node().end_byte(),
        text.len_bytes(),
        "the edited tree spans the new text"
    );
    // A fork reparses elsewhere and matches a fresh parse; the original
    // can also finish on its own.
    let mut fork = syntax.fork();
    let worker = {
        let text = text.clone();
        std::thread::spawn(move || {
            fork.reparse(&text);
            fork
        })
    };
    let fork = worker.join().unwrap();
    assert_same_as_fresh(&fork, &text, "forked reparse");
    assert!(syntax.reparse_within(&text, Duration::from_secs(60)));
    assert_same_as_fresh(&syntax, &text, "reparse after a stopped one");
}

#[test]
fn queries_compile_on_demand_or_ahead() {
    Language::Json.prepare();
    assert!(Language::Json.is_prepared());
}
