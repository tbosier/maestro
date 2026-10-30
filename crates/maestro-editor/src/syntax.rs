//! Per-buffer syntax state and keeping it in step with the text.
//!
//! Every path that changes a document's text calls
//! [`Editor::on_text_changed`] with the text from before the change and the
//! change sets applied, so the tree is updated incrementally.
//!
//! Parsing never blocks editing for long. Opening a small file parses it on
//! the main thread within [`OPEN_BUDGET`]; anything bigger or slower is
//! parsed on a background thread. After an edit the main thread reparses
//! within [`REPARSE_BUDGET`]; when that is not enough (very large files, or an
//! edit such as an unclosed quote that changes everything after it), the
//! reparse moves to a background thread. Meanwhile the screen keeps using the
//! previous tree with the edits applied to it (positions shift correctly,
//! new text is not parsed yet), and edits made while a background parse runs
//! are replayed onto its result when it arrives.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use maestro_core::{ChangeSet, Rope, Selection};
use maestro_syntax::{Edit, Highlight, Language, Syntax, input_edits};

use crate::editor::{DocumentId, Editor, ViewId};

/// Files up to this size get a parse attempt on the main thread when
/// opened, bounded by [`OPEN_BUDGET`], so the first frame has a tree.
pub const SYNC_PARSE_BYTES: usize = 256 * 1024;

/// Longest the main thread spends on a parse when a file is opened.
pub const OPEN_BUDGET: Duration = Duration::from_millis(50);

/// Longest the main thread spends reparsing after an edit. Tree-sitter
/// checks the clock only every so often, so the real cost can overshoot a
/// little; the 16 ms frame target leaves room for that.
pub const REPARSE_BUDGET: Duration = Duration::from_millis(4);

/// Files above this size get no syntax tree at all.
pub const MAX_PARSE_BYTES: usize = 64 << 20;

/// An edit that deletes plus inserts more than this many bytes (a reload,
/// select-all-and-delete, a huge paste) is not applied to the tree: walking
/// the old tree for it can take as long as a parse, and a single long token
/// (a pasted string) cannot be interrupted by the budget. The buffer is
/// parsed again from scratch instead, off the main thread if it is big.
pub const BIG_EDIT_BYTES: usize = 256 * 1024;

/// Highlight results kept per buffer, one per visible range, so split
/// views on one buffer do not evict each other.
const CACHE_ENTRIES: usize = 4;

static NEXT_PARSE: AtomicU64 = AtomicU64::new(1);

/// Syntax state for one buffer.
#[derive(Default)]
pub struct BufferSyntax {
    language: Option<Language>,
    /// The tree. Exact when no background parse is pending; otherwise the
    /// last tree with later edits applied, used only for drawing.
    syntax: Option<Syntax>,
    pending: Option<PendingParse>,
    /// The last reparse took longer than [`REPARSE_BUDGET`], so the next one
    /// goes straight to a background thread.
    slow: bool,
    /// Byte length of the text the tree describes. Highlights are withheld
    /// if it disagrees with the document, so a missed update path shows up
    /// as missing color rather than wrong color or a panic.
    synced_len: usize,
    note: Option<&'static str>,
    /// Bumped whenever the tree changes; keys the highlight cache.
    generation: u64,
    /// Recent highlights by visible range, so frames that change neither
    /// the tree nor the range (cursor motion, most commands) reuse them.
    cache: Mutex<Vec<HighlightCache>>,
    /// Per view: the selections before each `expand_selection`, and the
    /// selection the last expansion produced. `shrink_selection` walks back
    /// only while the view still has that selection.
    pub(crate) expansions: HashMap<ViewId, (Vec<Selection>, Selection)>,
}

struct HighlightCache {
    generation: u64,
    range: std::ops::Range<usize>,
    spans: Vec<Highlight>,
}

/// A background parse in flight and the edits made since it started.
struct PendingParse {
    id: u64,
    edits: Vec<Edit>,
}

/// What a background parse starts from.
enum ParseJob {
    Fresh(Language),
    Incremental(Syntax),
}

impl BufferSyntax {
    pub fn language(&self) -> Option<Language> {
        self.language
    }

    /// The tree, when it exactly matches the text (no parse pending).
    pub fn syntax(&self) -> Option<&Syntax> {
        self.syntax.as_ref().filter(|_| self.pending.is_none())
    }

    /// True while a background parse is running.
    pub fn is_parsing(&self) -> bool {
        self.pending.is_some()
    }

    /// Why a recognized language has no tree, if there is a reason to show.
    pub fn note(&self) -> Option<&'static str> {
        self.note
    }

    /// Highlights for the char range `range` of `text`. Empty while there is
    /// no tree, while the language's highlight query is still compiling, or
    /// if the tree does not match the text.
    pub fn highlights(&self, text: &Rope, range: std::ops::Range<usize>) -> Vec<Highlight> {
        let Some(s) = &self.syntax else {
            return Vec::new();
        };
        if self.synced_len != text.len_bytes() || !s.language().is_prepared() {
            return Vec::new();
        }
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|c| c.generation == self.generation);
        if let Some(c) = cache.iter().find(|c| c.range == range) {
            return c.spans.clone();
        }
        let spans = s.highlights(text, range.clone());
        if cache.len() >= CACHE_ENTRIES {
            cache.remove(0);
        }
        cache.push(HighlightCache {
            generation: self.generation,
            range,
            spans: spans.clone(),
        });
        spans
    }

    /// A status line message for when a command needs a tree and there is
    /// none.
    pub fn missing_reason(&self) -> String {
        match (self.language, self.pending.is_some(), self.note) {
            (None, _, _) => "no syntax tree: file type not recognized".into(),
            (Some(l), true, _) => format!("{} syntax tree is still being built", l.name()),
            (Some(_), false, Some(note)) => note.into(),
            (Some(l), false, None) => format!("no {} syntax tree", l.name()),
        }
    }
}

/// Bytes a tree covers, to decide where to free it.
fn tree_bytes(s: &Syntax) -> usize {
    s.tree().root_node().end_byte()
}

/// Bytes deleted plus inserted by `edits`.
fn edit_bytes(edits: &[Edit]) -> usize {
    edits
        .iter()
        .map(|e| (e.old_end_byte - e.start_byte) + (e.new_end_byte - e.start_byte))
        .sum()
}

/// Frees a replaced tree. Releasing a big tree visits every node it does
/// not share with its successor, which takes milliseconds on multi-megabyte
/// files, so trees over [`SYNC_PARSE_BYTES`] are freed on a short-lived
/// thread.
fn drop_elsewhere<T: Send + 'static>(old: T, bytes: usize) {
    if bytes > SYNC_PARSE_BYTES {
        // If the thread cannot start, the closure (and the tree) is dropped
        // here instead.
        let _ = std::thread::Builder::new()
            .name("maestro-syntax-free".into())
            .spawn(move || drop(old));
    }
}

impl Editor {
    /// Detects the language of buffer `id` from its path or shebang and
    /// parses it, unless it already has a tree for that language. Called
    /// when a document is added and when it is saved under a new name.
    pub fn refresh_syntax(&mut self, id: DocumentId) {
        let Some(buf) = self.buffers.get(&id) else {
            return;
        };
        let text = buf.doc.text();
        let first_line: String = text.line(0).chars().take(256).collect();
        let lang = Language::detect(buf.doc.path(), &first_line);
        let st = &buf.syntax;
        if lang == st.language && (lang.is_none() || st.syntax.is_some() || st.pending.is_some()) {
            return;
        }
        self.start_parse(id, lang);
    }

    /// Starts over for buffer `id` with language `lang`.
    fn start_parse(&mut self, id: DocumentId, lang: Option<Language>) {
        let Some(buf) = self.buffers.get_mut(&id) else {
            return;
        };
        let old = std::mem::replace(
            &mut buf.syntax,
            BufferSyntax {
                language: lang,
                ..BufferSyntax::default()
            },
        );
        let old_bytes = old.syntax.as_ref().map_or(0, tree_bytes);
        drop_elsewhere(old, old_bytes);
        let Some(lang) = lang else {
            return;
        };
        if !lang.is_prepared() {
            // Compiling a highlight query takes up to ~15 ms (Rust, TSX);
            // start it once per language on another thread, in parallel
            // with the parse below. The empty job makes the front end redraw
            // when it is done.
            let _ = self.spawn_background("syntax-query", move |tx| {
                lang.prepare();
                tx.send(|_| {});
            });
        }
        let buf = self.buffers.get_mut(&id).expect("checked above");
        let text = buf.doc.text();
        let bytes = text.len_bytes();
        if bytes > MAX_PARSE_BYTES {
            buf.syntax.note = Some("syntax highlighting is off for files over 64 MB");
            return;
        }
        if bytes <= SYNC_PARSE_BYTES
            && let Some(s) = Syntax::parse_within(lang, text, OPEN_BUDGET)
        {
            buf.syntax.syntax = Some(s);
            buf.syntax.synced_len = bytes;
            // Wait for the query too, so the first frame is colored. This
            // costs at most the compile time left over after the parse, once
            // per language.
            lang.prepare();
            return;
        }
        self.spawn_parse(id, ParseJob::Fresh(lang));
    }

    /// Parses buffer `id` on a background thread.
    fn spawn_parse(&mut self, id: DocumentId, job: ParseJob) {
        let Some(buf) = self.buffers.get_mut(&id) else {
            return;
        };
        let text = buf.doc.text().clone();
        let parse = NEXT_PARSE.fetch_add(1, Ordering::Relaxed);
        buf.syntax.pending = Some(PendingParse {
            id: parse,
            edits: Vec::new(),
        });
        let spawned = self.spawn_background("syntax", move |tx| {
            let start = Instant::now();
            let result = match job {
                ParseJob::Fresh(lang) => Syntax::parse(lang, &text),
                ParseJob::Incremental(mut s) => {
                    s.reparse(&text);
                    Some(s)
                }
            };
            let took = start.elapsed();
            tx.send(move |ed| ed.finish_parse(id, parse, result, took));
        });
        if spawned.is_err()
            && let Some(buf) = self.buffers.get_mut(&id)
        {
            // Without the worker the kept tree is stale; drop it rather than
            // let commands treat it as exact.
            buf.syntax.pending = None;
            buf.syntax.syntax = None;
            buf.syntax.note = Some("could not start the syntax thread");
        }
    }

    /// Installs a tree parsed in the background, after replaying the edits
    /// made while it was being built. A result for a parse that has since
    /// been superseded is dropped.
    fn finish_parse(&mut self, id: DocumentId, parse: u64, result: Option<Syntax>, took: Duration) {
        let result_bytes = result.as_ref().map_or(0, tree_bytes);
        let Some(buf) = self.buffers.get_mut(&id) else {
            drop_elsewhere(result, result_bytes);
            return;
        };
        let st = &mut buf.syntax;
        if st.pending.as_ref().map(|p| p.id) != Some(parse) {
            drop_elsewhere(result, result_bytes);
            return;
        }
        let pending = st.pending.take().expect("checked above");
        let Some(mut syntax) = result else {
            // Keep no stale tree around that would pass for an exact one.
            let old = st.syntax.take();
            let bytes = old.as_ref().map_or(0, tree_bytes);
            drop_elsewhere(old, bytes);
            st.note = Some("the syntax parser failed on this file");
            return;
        };
        st.slow = took > REPARSE_BUDGET;
        if edit_bytes(&pending.edits) > BIG_EDIT_BYTES {
            // Replaying would cost as much as parsing again.
            drop_elsewhere(syntax, result_bytes);
            let lang = st.language;
            self.start_parse(id, lang);
            return;
        }
        let replay = !pending.edits.is_empty();
        syntax.apply_edits(&pending.edits);
        let old = st.syntax.replace(syntax);
        let old_bytes = old.as_ref().map_or(0, tree_bytes);
        drop_elsewhere(old, old_bytes);
        st.generation += 1;
        st.synced_len = buf.doc.text().len_bytes();
        if replay {
            self.reparse(id);
        }
    }

    /// Reparses buffer `id`'s edited tree: here if it fits the budget,
    /// otherwise on a background thread.
    fn reparse(&mut self, id: DocumentId) {
        let Some(buf) = self.buffers.get_mut(&id) else {
            return;
        };
        let st = &mut buf.syntax;
        let Some(s) = &mut st.syntax else {
            return;
        };
        st.generation += 1;
        if !st.slow
            && let Some(old) = s.reparse_within(buf.doc.text(), REPARSE_BUDGET)
        {
            let bytes = old.root_node().end_byte();
            drop_elsewhere(old, bytes);
            return;
        }
        st.slow = true;
        let fork = s.fork();
        self.spawn_parse(id, ParseJob::Incremental(fork));
    }

    /// Keeps buffer `id`'s syntax tree in step with its text. `old` is the
    /// text before `changes` were applied, in order. Every code path that
    /// changes a document's text must call this.
    pub fn on_text_changed(&mut self, id: DocumentId, old: &Rope, changes: &[ChangeSet]) {
        let Some(buf) = self.buffers.get_mut(&id) else {
            return;
        };
        let st = &mut buf.syntax;
        st.expansions.clear();
        let edits = input_edits(old, changes);
        // Without a language from the file name, the first line (a shebang)
        // decides, so editing it can switch languages.
        let shebang_decides = buf.doc.path().and_then(Language::from_path).is_none();
        if shebang_decides && edits.iter().any(|e| e.start_position.row == 0) {
            let first_line: String = buf.doc.text().line(0).chars().take(256).collect();
            let lang = Language::detect(None, &first_line);
            if lang != st.language {
                self.start_parse(id, lang);
                return;
            }
        }
        let Some(lang) = st.language else {
            return;
        };
        let too_big = buf.doc.text().len_bytes() > MAX_PARSE_BYTES;
        if too_big
            || edit_bytes(&edits) > BIG_EDIT_BYTES
            || st.syntax.is_none() && st.pending.is_none()
        {
            // Parse from scratch: the edit is too big to apply to the tree,
            // the file crossed the size limit, or there was no tree (too
            // large before, or the parser failed). This re-checks the size.
            self.start_parse(id, Some(lang));
            return;
        }
        if let Some(s) = &mut st.syntax {
            s.apply_edits(&edits);
            st.synced_len = buf.doc.text().len_bytes();
            st.generation += 1;
        }
        if let Some(p) = &mut st.pending {
            p.edits.extend(edits);
            return;
        }
        self.reparse(id);
    }
}

#[cfg(test)]
mod tests {
    use maestro_config::Config;

    use super::*;
    use crate::key::parse_sequence;
    use crate::layout::Rect;

    fn settle(ed: &mut Editor) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while ed.poll_background() || ed.has_background_work() || ed.poll_background() {
            assert!(Instant::now() < deadline, "background work finishes");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn slow_reparses_move_off_the_main_thread_and_keep_shifted_colors() {
        let dir = std::env::temp_dir().join(format!("maestro-syntax-slow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.rs");
        std::fs::write(&path, "fn main() {\n    let x = 1;\n}\n").unwrap();
        let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 80, 24));
        let id = ed.open(&path).unwrap();
        settle(&mut ed);
        // Pretend the last reparse blew the budget.
        ed.buffers.get_mut(&id).unwrap().syntax.slow = true;

        ed.feed_keys(&parse_sequence("i space é").unwrap());
        let st = &ed.buffers[&id].syntax;
        assert!(st.is_parsing(), "the reparse went to a background thread");
        assert!(st.syntax().is_none(), "commands do not use the stale tree");
        // Drawing keeps the old tree, shifted past the two inserted chars.
        let text = ed.buffers[&id].doc.text().clone();
        assert!(text.to_string().starts_with(" éfn main"));
        let hl = st.highlights(&text, 0..text.len_chars());
        assert!(hl.contains(&(2, 4, "keyword")), "{hl:?}");

        settle(&mut ed);
        let st = &ed.buffers[&id].syntax;
        let fresh = Syntax::parse(Language::Rust, &text).unwrap();
        let tree = st.syntax().expect("tree is exact again");
        assert_eq!(
            tree.tree().root_node().to_sexp(),
            fresh.tree().root_node().to_sexp()
        );
        assert_eq!(
            st.highlights(&text, 0..text.len_chars()),
            fresh.highlights(&text, 0..text.len_chars())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
