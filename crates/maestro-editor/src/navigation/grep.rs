//! Workspace search: a regex over every non-ignored file, on background
//! threads, streamed into a picker as `path:line:col  text` items.
//!
//! The pattern uses the same smart case as in-buffer search: it is
//! case-insensitive unless it contains an uppercase letter. Files containing
//! a NUL byte are treated as binary and skipped, as are files larger than
//! [`MAX_FILE_BYTES`]. Files are searched as they are on disk; unsaved edits
//! in open buffers are not seen.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use ignore::WalkState;
use regex::bytes::{Regex, RegexBuilder};

use super::stream::{GAP, Stream};
use crate::editor::{Editor, JobSender};
use crate::picker::{PickerAction, PickerItem};

/// Results stop here; the title says when the cap was hit.
pub const RESULT_LIMIT: usize = 10_000;
/// Larger files are skipped.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Line text in result labels is cut to this many chars.
const LABEL_TEXT_CHARS: usize = 200;

/// Compiles a workspace search pattern with smart case.
pub fn compile(pattern: &str) -> Result<Regex, String> {
    let insensitive = !pattern.chars().any(char::is_uppercase);
    RegexBuilder::new(pattern)
        .case_insensitive(insensitive)
        .multi_line(true)
        .crlf(true)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineMatch {
    /// Zero-based line.
    pub line: usize,
    /// Zero-based char column of the first match on the line.
    pub column: usize,
    /// The line without its terminator.
    pub text: String,
}

/// Every line of `data` with a non-empty match, first match per line.
/// Returns `None` for binary data (a NUL byte anywhere). Stops after `limit`
/// lines.
pub fn search_bytes(data: &[u8], re: &Regex, limit: usize) -> Option<Vec<LineMatch>> {
    if data.contains(&0) {
        return None;
    }
    // A UTF-8 byte order mark is not part of the text the editor shows.
    let bom = if data.starts_with(b"\xEF\xBB\xBF") {
        3
    } else {
        0
    };
    let mut out = Vec::new();
    let mut line = 0;
    let mut line_start = bom;
    let mut scanned = bom;
    for m in re.find_iter(data) {
        if m.start() == m.end() || m.start() < bom {
            continue;
        }
        // Advance the line count to the match start.
        for (i, b) in data[scanned..m.start()].iter().enumerate() {
            if *b == b'\n' {
                line += 1;
                line_start = scanned + i + 1;
            }
        }
        scanned = m.start();
        if out.last().is_some_and(|l: &LineMatch| l.line == line) {
            continue;
        }
        let line_end = data[m.start()..]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(data.len(), |p| m.start() + p);
        let raw = &data[line_start..line_end];
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        out.push(LineMatch {
            line,
            column: String::from_utf8_lossy(&data[line_start..m.start()])
                .chars()
                .count(),
            text: String::from_utf8_lossy(raw).into_owned(),
        });
        if out.len() >= limit {
            break;
        }
    }
    Some(out)
}

fn item(label_path: &str, path: &Path, m: LineMatch) -> PickerItem {
    // Control characters (tabs included) would reach the terminal as-is.
    let mut text: String = m
        .text
        .trim()
        .chars()
        .take(LABEL_TEXT_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if m.text.trim().chars().count() > LABEL_TEXT_CHARS {
        text.push('…');
    }
    PickerItem {
        label: format!("{label_path}:{}:{}  {text}", m.line + 1, m.column + 1),
        detail: String::new(),
        action: PickerAction::Goto {
            path: path.to_path_buf(),
            line: m.line,
            column: m.column,
        },
    }
}

/// Starts a workspace search for `pattern` in the working directory.
pub fn start(ed: &mut Editor, pattern: &str) -> Result<(), String> {
    if pattern.is_empty() {
        return Err("workspace search needs a pattern".into());
    }
    let re = compile(pattern)?;
    let root = ed.cwd.clone();
    let title = format!("search /{pattern}/");
    let id = super::open_streaming_picker(ed, title.clone());
    let res = ed.spawn_background("workspace-search", move |tx| {
        search(root, re, tx, id, title);
    });
    if let Err(e) = res {
        ed.picker = None;
        return Err(format!("could not start the search: {e}"));
    }
    ed.registers.set('/', vec![pattern.to_string()]);
    Ok(())
}

/// `:grep <pattern>`. Arguments are joined with single spaces.
pub fn typed(ed: &mut Editor, args: &[&str], _force: bool) -> Result<(), String> {
    start(ed, &args.join(" "))
}

fn search(root: PathBuf, re: Regex, tx: JobSender, picker_id: u64, title: String) {
    let mut stream = Stream::new(tx, picker_id);
    // Stops the walker threads: set when the picker goes away or the result
    // cap is reached.
    let stop = AtomicBool::new(false);
    let (file_tx, file_rx) = mpsc::channel::<Vec<PickerItem>>();
    std::thread::scope(|scope| {
        let (re, stop_ref, root_ref) = (&re, &stop, &root);
        scope.spawn(move || walk(root_ref, re, file_tx, stop_ref));
        let mut capped = false;
        loop {
            match file_rx.recv_timeout(GAP) {
                Ok(items) => {
                    let room = RESULT_LIMIT - stream.count();
                    if items.len() >= room {
                        stream.extend(items.into_iter().take(room));
                        capped = true;
                        break;
                    }
                    stream.extend(items);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => stream.tick(),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if stream.cancelled() {
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        if capped {
            stream.finish(Some(format!("{title} (first {RESULT_LIMIT} matches)")));
        } else {
            stream.finish(None);
        }
    });
}

fn walk(root: &Path, re: &Regex, out: mpsc::Sender<Vec<PickerItem>>, stop: &AtomicBool) {
    super::finder::walker(root).build_parallel().run(|| {
        let out = out.clone();
        Box::new(move |entry| {
            if stop.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let path = entry.path();
            if entry.metadata().map_or(true, |m| m.len() > MAX_FILE_BYTES) {
                return WalkState::Continue;
            }
            let Ok(data) = std::fs::read(path) else {
                return WalkState::Continue;
            };
            let Some(matches) = search_bytes(&data, re, RESULT_LIMIT) else {
                return WalkState::Continue;
            };
            if matches.is_empty() {
                return WalkState::Continue;
            }
            let label = super::display_path(path, root);
            let items = matches.into_iter().map(|m| item(&label, path, m)).collect();
            if out.send(items).is_err() {
                return WalkState::Quit;
            }
            WalkState::Continue
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_columns_and_smart_case() {
        let re = compile("needle").unwrap();
        let data = "a needle\r\nnone\nαβ NEEDLE needle\n".as_bytes();
        let m = search_bytes(data, &re, 100).unwrap();
        assert_eq!(
            m,
            vec![
                LineMatch {
                    line: 0,
                    column: 2,
                    text: "a needle".into()
                },
                LineMatch {
                    line: 2,
                    column: 3,
                    text: "αβ NEEDLE needle".into()
                },
            ]
        );
        let re = compile("NEEDLE").unwrap();
        let m = search_bytes(data, &re, 100).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].line, m[0].column), (2, 3));
    }

    #[test]
    fn binary_bom_anchors_and_limit() {
        let re = compile("x").unwrap();
        assert_eq!(search_bytes(b"x\0x", &re, 100), None);
        let m = search_bytes("\u{feff}ax".as_bytes(), &re, 100).unwrap();
        assert_eq!((m[0].line, m[0].column), (0, 1));
        let re = compile("^b").unwrap();
        let m = search_bytes(b"ab\nbc\r\nbd", &re, 1).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line, 1);
        // Empty matches are not results.
        let re = compile("^").unwrap();
        assert!(search_bytes(b"a\nb", &re, 100).unwrap().is_empty());
    }

    #[test]
    fn item_label_is_one_based() {
        let it = item(
            "src/a.rs",
            Path::new("/r/src/a.rs"),
            LineMatch {
                line: 4,
                column: 2,
                text: "  let x = 1;".into(),
            },
        );
        assert_eq!(it.label, "src/a.rs:5:3  let x = 1;");
        assert_eq!(
            it.action,
            PickerAction::Goto {
                path: "/r/src/a.rs".into(),
                line: 4,
                column: 2
            }
        );
    }
}
