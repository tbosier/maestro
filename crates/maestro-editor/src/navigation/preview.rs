//! The picker preview: a window of lines around the selected item's file
//! location.
//!
//! It is loaded on the main thread when the selection changes, so reads are
//! bounded: at most [`MAX_SCAN_BYTES`] of a file are read to find the
//! window, and each line is cut to [`MAX_LINE_BYTES`]. A match further into
//! a file than that shows a message instead of text. Files already open in
//! a buffer are previewed from the buffer, unsaved edits included.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::editor::Editor;
use crate::picker::PickerAction;

/// The preview is shown (and loaded) only when the editor area is at least
/// this wide.
pub const MIN_WIDTH: u16 = 100;
/// Lines kept on each side of the target line.
pub const CONTEXT: usize = 40;
/// How far into a file the preview reads before giving up.
pub const MAX_SCAN_BYTES: u64 = 4 * 1024 * 1024;
/// Longest line kept, in bytes.
pub const MAX_LINE_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub path: PathBuf,
    /// The line to highlight (zero-based), if the item names one.
    pub target: Option<usize>,
    /// Line number of `lines[0]` (zero-based).
    pub first_line: usize,
    pub lines: Vec<String>,
    /// Shown instead of (or after) the lines: binary file, unreadable, too
    /// far into a large file.
    pub message: Option<String>,
    /// What the preview was built from: a buffer's document version or a
    /// file's modification time and length. A different stamp means the
    /// content may have changed, so the preview is rebuilt.
    pub stamp: u64,
}

/// The current stamp for `path`: the open buffer's version, else the file's
/// metadata (no read).
fn stamp_for(ed: &Editor, path: &Path) -> u64 {
    if let Some(id) = ed.find_buffer_by_path(path) {
        return ed.buffers[&id].doc.version().0;
    }
    std::fs::metadata(path)
        .map(|m| {
            let t = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos() as u64);
            t ^ m.len().rotate_left(32) ^ (1 << 63)
        })
        .unwrap_or(0)
}

/// The file location an action points at.
pub fn location(action: &PickerAction) -> Option<(&Path, Option<usize>)> {
    match action {
        PickerAction::OpenFile(p) => Some((p.as_path(), None)),
        PickerAction::Goto { path, line, .. } => Some((path.as_path(), Some(*line))),
        PickerAction::Buffer(_)
        | PickerAction::Command(_)
        | PickerAction::ChangeDir(_)
        | PickerAction::CodeAction(_)
        | PickerAction::Jump(_) => None,
    }
}

/// Window of lines `[from, to)` to load for a target line.
fn window(target: Option<usize>) -> (usize, usize) {
    match target {
        Some(t) => (t.saturating_sub(CONTEXT), t + CONTEXT + 1),
        None => (0, 2 * CONTEXT + 1),
    }
}

/// Reads the preview window from disk, bounded by [`MAX_SCAN_BYTES`].
pub fn load_file(path: &Path, target: Option<usize>) -> Preview {
    let (from, to) = window(target);
    let mut preview = Preview {
        stamp: 0,
        path: path.to_path_buf(),
        target,
        first_line: from,
        lines: Vec::new(),
        message: None,
    };
    // Opening a FIFO or a device would block the editor, so only regular
    // files are read.
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => {}
        Ok(_) => {
            preview.message = Some("not a regular file".into());
            return preview;
        }
        Err(e) => {
            preview.message = Some(format!("cannot read: {e}"));
            return preview;
        }
    }
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f.take(MAX_SCAN_BYTES),
        Err(e) => {
            preview.message = Some(format!("cannot read: {e}"));
            return preview;
        }
    };
    // Read in chunks until the window's last line is complete.
    let mut data = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut cur = super::lines::Cursor::default();
    loop {
        let n = match file.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                preview.message = Some(format!("cannot read: {e}"));
                break;
            }
        };
        if n == 0 {
            break;
        }
        if chunk[..n].contains(&0) {
            preview.message = Some("binary file".into());
            return preview;
        }
        data.extend_from_slice(&chunk[..n]);
        // Leave the last bytes for the next round: a CRLF or a multi-byte
        // separator may continue in the next chunk.
        let safe = data.len().saturating_sub(2);
        if safe > cur.scanned {
            cur = super::lines::advance(&data, cur, safe);
        }
        if cur.line >= to {
            break;
        }
    }
    let capped = data.len() as u64 >= MAX_SCAN_BYTES;
    // The editor hides a UTF-8 byte order mark; so does the preview.
    let text = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&data);
    if text.is_empty() && preview.message.is_none() {
        preview.message = Some("empty file".into());
        return preview;
    }
    let mut total = 0;
    for (i, (a, b)) in super::lines::lines(text).enumerate() {
        if i >= to {
            break;
        }
        total = i + 1;
        if i >= from {
            preview.lines.push(clean_line(&text[a..b]));
        }
    }
    if capped && target.is_some_and(|t| t + 1 >= total) {
        preview.message = Some(format!(
            "line {} is past the first {} MB this preview reads",
            target.unwrap_or(0) + 1,
            MAX_SCAN_BYTES / (1024 * 1024)
        ));
    }
    preview
}

/// Cuts a line (without its terminator) to [`MAX_LINE_BYTES`].
fn clean_line(raw: &[u8]) -> String {
    let cut = &raw[..raw.len().min(MAX_LINE_BYTES)];
    String::from_utf8_lossy(cut).into_owned()
}

/// Builds the preview from an open buffer's text.
fn load_rope(path: &Path, text: &maestro_core::Rope, target: Option<usize>) -> Preview {
    let (from, to) = window(target);
    let n = text.len_lines();
    let lines = (from.min(n)..to.min(n))
        .map(|i| {
            let l = text.line(i);
            let mut s = String::new();
            for chunk in l.chunks() {
                s.push_str(chunk);
                if s.len() >= MAX_LINE_BYTES {
                    break;
                }
            }
            let s = s.trim_end_matches(maestro_core::text::is_line_ending_char);
            let mut cut = s.len().min(MAX_LINE_BYTES);
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            s[..cut].to_string()
        })
        .collect();
    Preview {
        stamp: 0,
        path: path.to_path_buf(),
        target,
        first_line: from,
        lines,
        message: None,
    }
}

/// Recomputes the open picker's preview if its selection moved to a
/// different location. Clears it when the screen is too narrow to show one.
pub fn refresh(ed: &mut Editor) {
    let wide = ed.area.width >= MIN_WIDTH;
    let Some(picker) = ed.picker.as_ref() else {
        return;
    };
    let want = picker
        .selected_item()
        .and_then(|i| location(&i.action))
        .filter(|_| wide)
        .map(|(p, t)| (p.to_path_buf(), t));
    let Some((path, target)) = want else {
        if let Some(p) = ed.picker.as_mut() {
            p.preview = None;
        }
        return;
    };
    let stamp = stamp_for(ed, &path);
    if picker
        .preview
        .as_ref()
        .is_some_and(|p| p.path == path && p.target == target && p.stamp == stamp)
    {
        return;
    }
    let mut preview = match ed.find_buffer_by_path(&path) {
        Some(id) => load_rope(&path, ed.buffers[&id].doc.text(), target),
        None => load_file(&path, target),
    };
    preview.stamp = stamp;
    if let Some(p) = ed.picker.as_mut() {
        p.preview = Some(preview);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, contents: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("maestro-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn window_around_target() {
        let text: String = (0..200).map(|i| format!("line {i}\r\n")).collect();
        let p = temp_file("window.txt", text.as_bytes());
        let pv = load_file(&p, Some(100));
        assert_eq!(pv.first_line, 100 - CONTEXT);
        assert_eq!(pv.lines.len(), 2 * CONTEXT + 1);
        assert_eq!(pv.lines[CONTEXT], "line 100");
        assert_eq!(pv.message, None);
        let pv = load_file(&p, None);
        assert_eq!(pv.first_line, 0);
        assert_eq!(pv.lines[0], "line 0");
        let pv = load_file(&p, Some(199));
        // Like the rope, a final line break is followed by an empty line.
        assert_eq!(pv.lines[pv.lines.len() - 2], "line 199");
        assert_eq!(pv.lines.last().unwrap(), "");
    }

    #[test]
    fn lines_split_like_the_editor_and_bom_is_hidden() {
        let p = temp_file("cr.txt", "\u{feff}one\rtwo\u{2028}three\r\nfour".as_bytes());
        let pv = load_file(&p, Some(2));
        assert_eq!(pv.lines, vec!["one", "two", "three", "four"]);
        assert_eq!(pv.message, None);
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_not_opened() {
        let dir = std::env::temp_dir().join(format!("maestro-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("fifo");
        let _ = std::fs::remove_file(&fifo);
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("mkfifo is not available; skipping");
            return;
        }
        // Without the check this would block until a writer appeared.
        let pv = load_file(&fifo, None);
        assert_eq!(pv.message.as_deref(), Some("not a regular file"));
    }

    #[test]
    fn binary_and_missing_files_say_so() {
        let p = temp_file("bin.dat", b"abc\0def\n");
        assert_eq!(load_file(&p, None).message.as_deref(), Some("binary file"));
        let pv = load_file(Path::new("/definitely/not/here"), Some(3));
        assert!(pv.message.unwrap().starts_with("cannot read"));
    }

    #[test]
    fn large_files_stop_at_the_scan_cap() {
        let line = "x".repeat(99) + "\n";
        let n = (MAX_SCAN_BYTES as usize / line.len()) + 1000;
        let text = line.repeat(n);
        let p = temp_file("big.txt", text.as_bytes());
        let pv = load_file(&p, Some(n - 10));
        assert!(pv.message.unwrap().contains("past the first 4 MB"));
        let pv = load_file(&p, Some(10));
        assert_eq!(pv.message, None);
        assert_eq!(pv.lines[10].len(), 99);
    }

    #[test]
    fn rope_preview_cuts_long_lines() {
        let long = "é".repeat(MAX_LINE_BYTES);
        let text = maestro_core::Rope::from_str(&format!("a\n{long}\nc\n"));
        let pv = load_rope(Path::new("x"), &text, Some(1));
        assert_eq!(pv.lines[0], "a");
        assert!(pv.lines[1].len() <= MAX_LINE_BYTES);
        assert_eq!(pv.lines[2], "c");
    }
}
