//! Tree-sitter queries that select syntax nodes, for structural pipelines.
//!
//! A [`NodeQuery`] is a compiled query plus the capture that names the
//! nodes to select. [`Syntax::query_nodes`] runs it over a whole tree and
//! returns the char ranges of the captured nodes. Named shortcuts
//! ([`shortcuts`]) are ordinary queries kept here so they are tested
//! against each grammar.

use std::ops::Range;

use maestro_core::Rope;
use tree_sitter::{Node, Query, QueryCursor, QueryErrorKind, StreamingIterator};

use crate::{Language, Syntax};

/// The capture name that marks what to select when a query has several.
pub const SELECT_CAPTURE: &str = "select";

/// A compiled query and the capture whose nodes it selects.
pub struct NodeQuery {
    language: Language,
    query: Query,
    capture: u32,
}

impl std::fmt::Debug for NodeQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeQuery")
            .field("language", &self.language)
            .field("capture", &self.capture)
            .finish_non_exhaustive()
    }
}

/// Why a query did not compile, with its position in the query text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    /// Zero-based line in the query text.
    pub row: usize,
    /// Zero-based char column in that line.
    pub column: usize,
    pub message: String,
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.row > 0 {
            write!(
                f,
                "invalid query at line {}, column {}: {}",
                self.row + 1,
                self.column + 1,
                self.message
            )
        } else {
            write!(
                f,
                "invalid query at column {}: {}",
                self.column + 1,
                self.message
            )
        }
    }
}

impl std::error::Error for QueryError {}

impl NodeQuery {
    /// Compiles `source` for `language`. The nodes selected are those of the
    /// capture named `@select`; without one, the query's only capture; and
    /// for a single pattern with no capture at all, the whole pattern (as if
    /// it ended in `@select`).
    ///
    /// Only the predicates tree-sitter evaluates itself are accepted:
    /// `#eq?`, `#match?`, `#any-of?` and their `#not-` and `#any-` forms.
    /// Any other (a misspelled `#equal?`, `#is?`) is refused, because it
    /// would be ignored and the query would match more than it says.
    pub fn new(language: Language, source: &str) -> Result<NodeQuery, QueryError> {
        let grammar = language.grammar();
        let query = Query::new(&grammar, source).map_err(|e| convert_error(source, &e))?;
        check_predicates(source, &query)?;
        let names = query.capture_names();
        if let Some(i) = names.iter().position(|n| *n == SELECT_CAPTURE) {
            return Ok(Self::with_capture(language, query, i));
        }
        match names.len() {
            1 => Ok(Self::with_capture(language, query, 0)),
            0 if query.pattern_count() == 1 => {
                // On its own line, so a trailing `;` comment cannot hide it.
                let wrapped = format!("{}\n@{SELECT_CAPTURE}", source.trim_end());
                let query =
                    Query::new(&grammar, &wrapped).map_err(|e| convert_error(&wrapped, &e))?;
                Ok(Self::with_capture(language, query, 0))
            }
            0 => Err(QueryError {
                row: 0,
                column: 0,
                message: "the query has several patterns and no capture; add @select after the node to select".into(),
            }),
            _ => Err(QueryError {
                row: 0,
                column: 0,
                message: format!(
                    "the query has captures {}; name the one to select @{SELECT_CAPTURE}",
                    names
                        .iter()
                        .map(|n| format!("@{n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }),
        }
    }

    /// Compiles the named shortcut `name` for `language`, or `None` if the
    /// language has no shortcut by that name.
    pub fn shortcut(language: Language, name: &str) -> Option<NodeQuery> {
        let (_, _, source) = shortcuts(language).iter().find(|(n, _, _)| *n == name)?;
        Some(NodeQuery::new(language, source).expect("bundled shortcuts compile (tested)"))
    }

    fn with_capture(language: Language, query: Query, index: usize) -> NodeQuery {
        NodeQuery {
            language,
            query,
            capture: u32::try_from(index).expect("capture count fits in u32"),
        }
    }

    pub fn language(&self) -> Language {
        self.language
    }
}

/// Refuses predicates that tree-sitter does not evaluate during matching.
fn check_predicates(source: &str, query: &Query) -> Result<(), QueryError> {
    for pattern in 0..query.pattern_count() {
        let unsupported = query
            .general_predicates(pattern)
            .iter()
            .map(|p| p.operator.to_string())
            .chain(
                query
                    .property_predicates(pattern)
                    .iter()
                    .map(|(_, positive)| if *positive { "is?" } else { "is-not?" }.to_string()),
            )
            .next();
        if let Some(op) = unsupported {
            // Point at the predicate where it is written, if it can be found.
            let (row, column) = source
                .find(&format!("#{op}"))
                .map_or((0, 0), |at| position(source, at));
            return Err(QueryError {
                row,
                column,
                message: format!(
                    "unsupported predicate #{op} (supported: #eq?, #match?, #any-of? and their #not- and #any- forms)"
                ),
            });
        }
    }
    Ok(())
}

/// The zero-based line and char column of byte `offset` in `source`.
fn position(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let offset = (0..=offset)
        .rev()
        .find(|&i| source.is_char_boundary(i))
        .unwrap_or(0);
    let line_start = source[..offset].rfind('\n').map_or(0, |i| i + 1);
    let row = source[..offset].matches('\n').count();
    (row, source[line_start..offset].chars().count())
}

/// Turns tree-sitter's error (a byte offset and a kind) into a message with
/// a char column.
fn convert_error(source: &str, e: &tree_sitter::QueryError) -> QueryError {
    let (row, column) = position(source, e.offset);
    // Names arrive quoted for some kinds and bare for others.
    let name = e.message.trim_matches('"');
    let message = match e.kind {
        QueryErrorKind::Syntax => "syntax error".to_string(),
        QueryErrorKind::NodeType => format!("unknown node kind \"{name}\""),
        QueryErrorKind::Field => format!("unknown field \"{name}\""),
        QueryErrorKind::Capture => format!("unknown capture @{name}"),
        QueryErrorKind::Predicate => format!("bad predicate: {}", e.message),
        QueryErrorKind::Structure => "this pattern cannot match anything in the grammar".into(),
        QueryErrorKind::Language => format!("language error: {}", e.message),
    };
    QueryError {
        row,
        column,
        message,
    }
}

/// Named shortcuts for `language`: `(name, description, query)`.
pub fn shortcuts(language: Language) -> &'static [(&'static str, &'static str, &'static str)] {
    match language {
        Language::Rust => RUST,
        Language::Python => PYTHON,
        Language::JavaScript => JAVASCRIPT,
        Language::TypeScript | Language::Tsx => TYPESCRIPT,
        Language::C => C,
        Language::Toml | Language::Json | Language::Markdown => &[],
    }
}

const RUST: &[(&str, &str, &str)] = &[
    (
        "fn",
        "functions and methods with a body",
        "(function_item) @select",
    ),
    (
        "pub-fn",
        "functions declared `pub` (not `pub(crate)` and the like)",
        r#"(function_item (visibility_modifier) @_vis (#eq? @_vis "pub")) @select"#,
    ),
    (
        "async-fn",
        "async functions",
        r#"(function_item (function_modifiers "async")) @select"#,
    ),
    (
        "pub-async-fn",
        "async functions declared `pub`",
        r#"(function_item (visibility_modifier) @_vis (#eq? @_vis "pub") (function_modifiers "async")) @select"#,
    ),
    ("struct", "struct definitions", "(struct_item) @select"),
    ("enum", "enum definitions", "(enum_item) @select"),
    ("impl", "impl blocks", "(impl_item) @select"),
    ("trait", "trait definitions", "(trait_item) @select"),
    ("mod", "modules", "(mod_item) @select"),
    (
        "call",
        "function and method calls (not macros)",
        "(call_expression) @select",
    ),
    (
        "string",
        "string literals, raw ones included",
        "[(string_literal) (raw_string_literal)] @select",
    ),
    (
        "returns-result",
        "functions whose return type is `Result` or a path ending in `Result` (`io::Result`)",
        r#"(function_item return_type: [
            (type_identifier) @_t
            (scoped_type_identifier name: (type_identifier) @_t)
            (generic_type type: (type_identifier) @_t)
            (generic_type type: (scoped_type_identifier name: (type_identifier) @_t))
          ] (#eq? @_t "Result")) @select"#,
    ),
    (
        "instrumented",
        "functions with `#[instrument]` or `#[tracing::instrument]` (also `::tracing::` and raw identifiers), with or without arguments, among the attributes and comments right above them",
        r#"((attribute_item
             (attribute (identifier) @_name
               (#any-of? @_name "instrument" "r#instrument")))
           .
           [(attribute_item) (line_comment) (block_comment)]*
           .
           (function_item) @select)
          ((attribute_item
             (attribute
               (scoped_identifier
                 path: [(identifier) @_path (scoped_identifier !path name: (identifier) @_path)]
                 name: (identifier) @_name)
               (#any-of? @_path "tracing" "r#tracing")
               (#any-of? @_name "instrument" "r#instrument")))
           .
           [(attribute_item) (line_comment) (block_comment)]*
           .
           (function_item) @select)"#,
    ),
];

const PYTHON: &[(&str, &str, &str)] = &[
    (
        "fn",
        "functions and methods",
        "(function_definition) @select",
    ),
    (
        "async-fn",
        "async functions",
        r#"(function_definition "async") @select"#,
    ),
    ("class", "class definitions", "(class_definition) @select"),
    ("call", "calls", "(call) @select"),
    ("string", "string literals", "(string) @select"),
];

const JAVASCRIPT: &[(&str, &str, &str)] = &[
    (
        "fn",
        "function declarations and expressions, arrow functions and methods",
        "[(function_declaration) (generator_function_declaration) (function_expression) (generator_function) (arrow_function) (method_definition)] @select",
    ),
    (
        "class",
        "class declarations and expressions",
        "[(class_declaration) (class)] @select",
    ),
    ("call", "calls", "(call_expression) @select"),
    (
        "string",
        "string and template literals",
        "[(string) (template_string)] @select",
    ),
];

const TYPESCRIPT: &[(&str, &str, &str)] = &[
    (
        "fn",
        "function declarations and expressions, arrow functions and methods",
        "[(function_declaration) (generator_function_declaration) (function_expression) (generator_function) (arrow_function) (method_definition)] @select",
    ),
    (
        "class",
        "class declarations and expressions",
        "[(class_declaration) (abstract_class_declaration) (class)] @select",
    ),
    (
        "interface",
        "interface declarations",
        "(interface_declaration) @select",
    ),
    ("call", "calls", "(call_expression) @select"),
    (
        "string",
        "string and template literals",
        "[(string) (template_string)] @select",
    ),
];

const C: &[(&str, &str, &str)] = &[
    (
        "fn",
        "function definitions",
        "(function_definition) @select",
    ),
    (
        "struct",
        "struct definitions with a body",
        "(struct_specifier body: (field_declaration_list)) @select",
    ),
    ("call", "calls", "(call_expression) @select"),
    ("string", "string literals", "(string_literal) @select"),
];

impl Language {
    /// True if the grammar has a named node kind called `kind`.
    pub fn has_node_kind(self, kind: &str) -> bool {
        self.grammar().id_for_node_kind(kind, true) != 0
    }
}

impl Syntax {
    /// The char ranges of every node `query` selects in the whole tree,
    /// sorted by start (then by end, longest first) without duplicates.
    /// Empty nodes are left out. A query compiled for another language
    /// selects nothing.
    pub fn query_nodes(&self, text: &Rope, query: &NodeQuery) -> Vec<Range<usize>> {
        if query.language != self.language {
            return Vec::new();
        }
        let len_bytes = text.len_bytes();
        let provider = |node: Node| {
            let r = node.byte_range();
            text.byte_slice(r.start.min(len_bytes)..r.end.min(len_bytes))
                .chunks()
                .map(str::as_bytes)
        };
        let mut bytes: Vec<(usize, usize)> = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query.query, self.tree.root_node(), provider);
        while let Some(m) = matches.next() {
            for c in m.captures.iter().filter(|c| c.index == query.capture) {
                let r = c.node.byte_range();
                if r.start < r.end {
                    bytes.push((r.start, r.end));
                }
            }
        }
        bytes.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        bytes.dedup();
        bytes
            .into_iter()
            .map(|(a, b)| self.chars(text, a..b))
            .collect()
    }

    /// The char range of the smallest node whose kind is one of `kinds`
    /// and that contains `from..to` (it may equal it).
    pub fn enclosing_kind(
        &self,
        text: &Rope,
        from: usize,
        to: usize,
        kinds: &[&str],
    ) -> Option<Range<usize>> {
        let (fb, tb) = self.bytes(text, from, to);
        let mut node = self.tree.root_node().descendant_for_byte_range(fb, tb);
        while let Some(n) = node {
            let r = n.byte_range();
            if r.start <= fb && r.end >= tb && kinds.contains(&n.kind()) {
                return Some(self.chars(text, r));
            }
            node = n.parent();
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of every node `query` selects in `src`.
    fn select(lang: Language, src: &str, query: &str) -> Vec<String> {
        let text = Rope::from_str(src);
        let syntax = Syntax::parse(lang, &text).unwrap();
        let q = NodeQuery::shortcut(lang, query)
            .map(Ok)
            .unwrap_or_else(|| NodeQuery::new(lang, query))
            .unwrap();
        syntax
            .query_nodes(&text, &q)
            .into_iter()
            .map(|r| text.slice(r).to_string())
            .collect()
    }

    /// The name after `fn` in each selected function, for compact
    /// assertions.
    fn fn_names(src: &str, query: &str) -> Vec<String> {
        select(Language::Rust, src, query)
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

    const RUST_SRC: &str = r#"
#[instrument]
#[allow(dead_code)]
/// Docs.
pub async fn a() -> Result<(), ()> { Ok(()) }

#[tracing::instrument(skip(x))]
pub async fn b(x: u8) -> io::Result<()> { Ok(()) }

pub async fn c() -> std::result::Result<u8, ()> { Ok(1) }

#[instrument]
fn d() {}

pub async fn e() -> u32 {
    fn inner() -> Result<(), ()> { Ok(()) }
    1
}

pub(crate) async fn f() -> Result<u8, ()> { Ok(1) }

/// ```
/// pub async fn in_doc() -> Result<(), ()> { Ok(()) }
/// ```
pub fn g() -> Option<Result<(), ()>> { let _ = "pub async fn in_string() -> Result<(), ()>"; None }

#[instrumented]
pub async fn h() -> Result<(), ()> { Ok(()) }

impl X {
    pub async fn i(&self) -> Result<(), ()> { Ok(()) }
}
"#;

    #[test]
    fn rust_shortcuts_select_what_they_say() {
        let names = |q| fn_names(RUST_SRC, q);
        assert_eq!(
            names("fn"),
            ["a", "b", "c", "d", "e", "inner", "f", "g", "h", "i"]
        );
        assert_eq!(names("pub-fn"), ["a", "b", "c", "e", "g", "h", "i"]);
        assert_eq!(names("async-fn"), ["a", "b", "c", "e", "f", "h", "i"]);
        assert_eq!(names("pub-async-fn"), ["a", "b", "c", "e", "h", "i"]);
        assert_eq!(
            names("returns-result"),
            ["a", "b", "c", "inner", "f", "h", "i"]
        );
        assert_eq!(names("instrumented"), ["a", "b", "d"]);
        assert_eq!(
            select(Language::Rust, RUST_SRC, "string"),
            [r#""pub async fn in_string() -> Result<(), ()>""#]
        );
        assert_eq!(select(Language::Rust, RUST_SRC, "impl").len(), 1);
        assert_eq!(
            select(Language::Rust, RUST_SRC, "call"),
            [
                "Ok(())", "Ok(())", "Ok(1)", "Ok(())", "Ok(1)", "Ok(())", "Ok(())"
            ]
        );
        let src = "struct A; enum B { X } trait C {} mod d { fn e() {} }";
        assert_eq!(select(Language::Rust, src, "struct"), ["struct A;"]);
        assert_eq!(select(Language::Rust, src, "enum"), ["enum B { X }"]);
        assert_eq!(select(Language::Rust, src, "trait"), ["trait C {}"]);
        assert_eq!(select(Language::Rust, src, "mod"), ["mod d { fn e() {} }"]);
    }

    #[test]
    fn an_instrument_attribute_belongs_only_to_the_item_right_below_it() {
        let src = "#[instrument]\nfn a() {}\nfn b() {}\n#[instrument]\nstruct S;\nfn c() {}\n#[instrument] // why\n/* note */\n#[inline]\nfn d() {}\n";
        assert_eq!(fn_names(src, "instrumented"), ["a", "d"]);
    }

    #[test]
    fn other_languages_shortcuts_select_what_they_say() {
        let py = "import x\n@dec\ndef f(a):\n    return g(a, 'str')\n\nasync def h():\n    pass\n\nclass K:\n    def m(self):\n        pass\n";
        let first_lines = |v: Vec<String>| -> Vec<String> {
            v.iter()
                .map(|s| s.lines().next().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            first_lines(select(Language::Python, py, "fn")),
            ["def f(a):", "async def h():", "def m(self):"]
        );
        assert_eq!(
            first_lines(select(Language::Python, py, "async-fn")),
            ["async def h():"]
        );
        assert_eq!(
            first_lines(select(Language::Python, py, "class")),
            ["class K:"]
        );
        assert_eq!(select(Language::Python, py, "call"), ["g(a, 'str')"]);
        assert_eq!(select(Language::Python, py, "string"), ["'str'"]);

        let js = "function f() { return g(`t`, 'x'); }\nconst h = () => 1;\nclass K { m() {} }\n";
        for lang in [Language::JavaScript, Language::TypeScript, Language::Tsx] {
            assert_eq!(
                select(lang, js, "fn"),
                ["function f() { return g(`t`, 'x'); }", "() => 1", "m() {}"],
                "{lang:?}"
            );
            assert_eq!(select(lang, js, "class"), ["class K { m() {} }"]);
            assert_eq!(select(lang, js, "call"), ["g(`t`, 'x')"]);
            assert_eq!(select(lang, js, "string"), ["`t`", "'x'"]);
        }
        let ts = "interface I { a: number }\nabstract class A {}\n";
        assert_eq!(
            select(Language::TypeScript, ts, "interface"),
            ["interface I { a: number }"]
        );
        assert_eq!(
            select(Language::TypeScript, ts, "class"),
            ["abstract class A {}"]
        );

        let c = "struct P;\nstruct Q { int x; };\nint f(void) { return g(\"s\"); }\n";
        assert_eq!(
            select(Language::C, c, "fn"),
            ["int f(void) { return g(\"s\"); }"]
        );
        assert_eq!(select(Language::C, c, "struct"), ["struct Q { int x; }"]);
        assert_eq!(select(Language::C, c, "call"), ["g(\"s\")"]);
        assert_eq!(select(Language::C, c, "string"), ["\"s\""]);
    }

    #[test]
    fn instrument_attributes_are_matched_by_path_not_by_spelling() {
        let src = "#[tracing :: instrument]\nfn a() {}\n#[::tracing::instrument(skip(x))]\nfn b() {}\n#[r#tracing::r#instrument]\nfn c() {}\n#[tracing /* why */ :: instrument]\nfn d() {}\n#[other::tracing::instrument]\nfn e() {}\n#[tracing::instrumented]\nfn f() {}\n#[my::instrument]\nfn g() {}\n";
        assert_eq!(fn_names(src, "instrumented"), ["a", "b", "c", "d"]);
    }

    #[test]
    fn predicates_tree_sitter_ignores_are_refused() {
        for q in [
            r#"(function_item name: (identifier) @n (#equal? @n "x")) @select"#,
            r#"(function_item name: (identifier) @n (#is? @n local)) @select"#,
        ] {
            let e = NodeQuery::new(Language::Rust, q).unwrap_err();
            assert!(e.message.starts_with("unsupported predicate #"), "{e}");
            assert_eq!(e.column, 38, "{e}");
        }
        let src = "fn wanted() {}\nfn innocent() {}\n";
        for q in [
            r#"(function_item name: (identifier) @n (#eq? @n "wanted")) @select"#,
            r#"(function_item name: (identifier) @n (#match? @n "^want")) @select"#,
            r#"(function_item name: (identifier) @n (#any-of? @n "wanted" "x")) @select"#,
        ] {
            assert_eq!(fn_names(src, q), ["wanted"], "{q}");
        }
        assert_eq!(
            fn_names(
                src,
                r#"(function_item name: (identifier) @n (#not-eq? @n "wanted")) @select"#
            ),
            ["innocent"]
        );
    }

    #[test]
    fn a_trailing_comment_does_not_hide_the_implied_capture() {
        assert_eq!(
            select(
                Language::Rust,
                "fn f() {}",
                "(function_item) ; the functions"
            ),
            ["fn f() {}"]
        );
    }

    #[test]
    fn javascript_fn_includes_generator_expressions() {
        let js = "const f = function* () { yield 1; };\nfunction* g() {}\n";
        for lang in [Language::JavaScript, Language::TypeScript, Language::Tsx] {
            assert_eq!(
                select(lang, js, "fn"),
                ["function* () { yield 1; }", "function* g() {}"],
                "{lang:?}"
            );
        }
    }

    #[test]
    fn every_shortcut_compiles_for_its_language() {
        for lang in Language::ALL {
            for (name, _, _) in shortcuts(lang) {
                assert!(NodeQuery::shortcut(lang, name).is_some(), "{lang:?} {name}");
            }
        }
    }

    #[test]
    fn capture_rules_and_error_positions() {
        let src = "fn a() { b(); }";
        // No capture: the single pattern is selected.
        assert_eq!(select(Language::Rust, src, "(call_expression)"), ["b()"]);
        // One capture, whatever its name.
        assert_eq!(
            select(
                Language::Rust,
                src,
                "(call_expression function: (identifier) @f)"
            ),
            ["b"]
        );
        // Several: @select decides.
        assert_eq!(
            select(
                Language::Rust,
                src,
                "(function_item name: (identifier) @n body: (block) @select)"
            ),
            ["{ b(); }"]
        );
        let err = |q| NodeQuery::new(Language::Rust, q).unwrap_err();
        let e = err("(function_item name: (identifier) @n body: (block) @b)");
        assert!(e.message.contains("@n, @b"), "{e}");
        let e = err("(function_item) (call_expression)");
        assert!(e.message.contains("add @select"), "{e}");
        let e = err("(function_item (nope))");
        assert_eq!((e.row, e.column), (0, 16), "{e}");
        assert_eq!(
            e.to_string(),
            "invalid query at column 17: unknown node kind \"nope\""
        );
        let e = err("(function_item");
        assert!(e.to_string().starts_with("invalid query at column"), "{e}");
        assert!(e.message.contains("syntax error"), "{e}");
        let e = err("(function_item\n  bogus: (identifier))");
        assert_eq!((e.row, e.column), (1, 2), "{e}");
        assert_eq!(
            e.to_string(),
            "invalid query at line 2, column 3: unknown field \"bogus\""
        );
    }

    #[test]
    fn enclosing_kind_finds_the_smallest_containing_node() {
        let src = "impl X {\n    fn a() { b(c()); }\n}\n";
        let text = Rope::from_str(src);
        let syntax = Syntax::parse(Language::Rust, &text).unwrap();
        let at = src.find("c()").unwrap();
        let r = syntax
            .enclosing_kind(&text, at, at + 1, &["call_expression"])
            .unwrap();
        assert_eq!(&src[r], "c()");
        let r = syntax
            .enclosing_kind(&text, at, at + 1, &["function_item", "impl_item"])
            .unwrap();
        assert_eq!(&src[r.clone()], "fn a() { b(c()); }");
        // A node of the kind that equals the range is its own answer.
        assert_eq!(
            syntax.enclosing_kind(&text, r.start, r.end, &["function_item"]),
            Some(r)
        );
        assert_eq!(
            syntax.enclosing_kind(&text, at, at + 1, &["struct_item"]),
            None
        );
        assert!(Language::Rust.has_node_kind("function_item"));
        assert!(!Language::Rust.has_node_kind("fn_item"));
    }
}
