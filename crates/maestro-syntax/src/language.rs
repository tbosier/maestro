//! Supported languages: detection from file names and shebangs, grammars and
//! highlight queries.

use std::path::Path;
use std::sync::OnceLock;

use tree_sitter::Query;

/// A language with a bundled tree-sitter grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    Toml,
    Json,
    Markdown,
    JavaScript,
    TypeScript,
    Tsx,
    C,
}

impl Language {
    pub const ALL: [Language; 9] = [
        Language::Rust,
        Language::Python,
        Language::Toml,
        Language::Json,
        Language::Markdown,
        Language::JavaScript,
        Language::TypeScript,
        Language::Tsx,
        Language::C,
    ];

    /// A short lowercase name, as shown in the status line.
    pub fn name(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::Toml => "toml",
            Language::Json => "json",
            Language::Markdown => "markdown",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::C => "c",
        }
    }

    /// Detects a language from a file name or extension.
    pub fn from_path(path: &Path) -> Option<Language> {
        let name = path.file_name()?.to_str()?;
        match name {
            "Cargo.lock" | "Pipfile" | "uv.lock" | "poetry.lock" => return Some(Language::Toml),
            _ => {}
        }
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Language::Rust,
            "py" | "pyi" | "pyw" => Language::Python,
            "toml" => Language::Toml,
            "json" => Language::Json,
            "md" | "markdown" => Language::Markdown,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "c" | "h" => Language::C,
            _ => return None,
        })
    }

    /// Detects a language from a `#!` line such as `#!/usr/bin/env python3`.
    pub fn from_shebang(first_line: &str) -> Option<Language> {
        let rest = first_line.strip_prefix("#!")?;
        let mut words = rest.split_whitespace();
        let mut program = words.next()?.rsplit('/').next()?;
        if program == "env" {
            // Skip `env` flags such as `-S`.
            program = words.find(|w| !w.starts_with('-'))?;
        }
        let program = program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
        match program {
            "python" | "pypy" => Some(Language::Python),
            "node" | "nodejs" | "deno" | "bun" => Some(Language::JavaScript),
            _ => None,
        }
    }

    /// Detects from the path first, then from the first line of the text.
    pub fn detect(path: Option<&Path>, first_line: &str) -> Option<Language> {
        path.and_then(Language::from_path)
            .or_else(|| Language::from_shebang(first_line))
    }

    pub(crate) fn grammar(self) -> tree_sitter::Language {
        match self {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::Python => tree_sitter_python::LANGUAGE.into(),
            Language::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Language::Json => tree_sitter_json::LANGUAGE.into(),
            Language::Markdown => tree_sitter_md::LANGUAGE.into(),
            Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Language::C => tree_sitter_c::LANGUAGE.into(),
        }
    }

    /// The highlight query source. TypeScript's bundled query only adds to
    /// JavaScript's, so the two are combined the way the grammar's own
    /// package metadata combines them (TypeScript patterns first).
    fn highlight_source(self) -> String {
        use tree_sitter_javascript as js;
        use tree_sitter_typescript as ts;
        match self {
            Language::Rust => tree_sitter_rust::HIGHLIGHTS_QUERY.into(),
            Language::Python => tree_sitter_python::HIGHLIGHTS_QUERY.into(),
            Language::Toml => tree_sitter_toml_ng::HIGHLIGHTS_QUERY.into(),
            Language::Json => tree_sitter_json::HIGHLIGHTS_QUERY.into(),
            Language::Markdown => tree_sitter_md::HIGHLIGHT_QUERY_BLOCK.into(),
            Language::JavaScript => [js::HIGHLIGHT_QUERY, js::JSX_HIGHLIGHT_QUERY].join("\n"),
            Language::TypeScript => [ts::HIGHLIGHTS_QUERY, js::HIGHLIGHT_QUERY].join("\n"),
            Language::Tsx => [
                ts::HIGHLIGHTS_QUERY,
                js::HIGHLIGHT_QUERY,
                js::JSX_HIGHLIGHT_QUERY,
            ]
            .join("\n"),
            Language::C => tree_sitter_c::HIGHLIGHT_QUERY.into(),
        }
    }

    /// The compiled highlight query, built on first use and shared after.
    pub(crate) fn highlights(self) -> &'static HighlightQuery {
        self.query_cell().get_or_init(|| HighlightQuery::new(self))
    }

    /// Compiles the highlight query now. Compiling takes tens of
    /// milliseconds for the larger queries, so editors call this on a
    /// background thread and check [`Language::is_prepared`] before drawing.
    pub fn prepare(self) {
        self.highlights();
    }

    /// True once the highlight query is compiled, so
    /// [`crate::Syntax::highlights`] will not block on compiling it.
    pub fn is_prepared(self) -> bool {
        self.query_cell().get().is_some()
    }

    fn query_cell(self) -> &'static OnceLock<HighlightQuery> {
        static CELLS: [OnceLock<HighlightQuery>; 9] = [const { OnceLock::new() }; 9];
        let idx = Language::ALL
            .iter()
            .position(|l| *l == self)
            .expect("every language is in ALL");
        &CELLS[idx]
    }
}

/// A compiled highlight query plus the theme scope for each capture.
pub(crate) struct HighlightQuery {
    pub(crate) query: Option<Query>,
    /// Theme scope per capture index; `None` for captures that only feed
    /// predicates (`_name`) or explicitly mean "no highlight" (`none`).
    pub(crate) scopes: Vec<Option<&'static str>>,
}

impl HighlightQuery {
    fn new(lang: Language) -> Self {
        let query = Query::new(&lang.grammar(), &lang.highlight_source()).ok();
        let scopes = match &query {
            Some(q) => (0..q.capture_names().len())
                .map(|i| capture_scope(q.capture_names()[i]))
                .collect(),
            None => Vec::new(),
        };
        HighlightQuery { query, scopes }
    }
}

/// Maps a capture name from the grammar's bundled query onto the theme's
/// nvim-treesitter / Helix scope names. Names the theme already knows pass
/// through unchanged.
fn capture_scope(name: &str) -> Option<&'static str> {
    if name.starts_with('_') || name == "none" || name.starts_with("injection.") {
        return None;
    }
    Some(match name {
        "text.title" => "markup.heading",
        "text.literal" => "markup.raw",
        "text.uri" => "markup.link.url",
        "text.reference" => "markup.link",
        "text.emphasis" => "markup.italic",
        "text.strong" => "markup.bold",
        "delimiter" => "punctuation.delimiter",
        other => intern(other),
    })
}

/// Leaks one copy of each distinct capture name so highlights can carry
/// `&'static str`. Bounded by the number of capture names across the bundled
/// queries (a few dozen), and only runs when a query is first compiled.
fn intern(name: &str) -> &'static str {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static NAMES: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let mut guard = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    let set = guard.get_or_insert_with(HashSet::new);
    if let Some(s) = set.get(name) {
        return s;
    }
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    set.insert(leaked);
    leaked
}
