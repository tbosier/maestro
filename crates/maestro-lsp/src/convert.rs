//! Conversions between editor coordinates and protocol JSON.
//!
//! Positions use UTF-16 code units, the protocol default, through
//! [`maestro_core::pos::char_to_lsp`] and [`maestro_core::pos::lsp_to_char`].

use std::path::{Path, PathBuf};

use maestro_core::pos::{LspPosition, char_to_lsp, lsp_to_char};
use maestro_core::{ChangeSet, Rope};
use serde_json::{Value, json};

/// A `file://` URI for an absolute path. Bytes outside the unreserved set
/// (and `/`) are percent-encoded.
pub fn path_to_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    let s = path.to_string_lossy();
    let s = s.replace('\\', "/");
    if !s.starts_with('/') {
        // A Windows drive path: file:///C:/...
        out.push('/');
    }
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else if b == b':' && cfg!(windows) {
            out.push(':');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The path a `file://` URI names, or `None` for other schemes and
/// malformed escapes.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Skip an authority ("localhost") if present.
    let rest = if rest.starts_with('/') {
        rest
    } else {
        &rest[rest.find('/')?..]
    };
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let s = String::from_utf8(out).ok()?;
    if cfg!(windows) {
        let s = s.strip_prefix('/').unwrap_or(&s);
        return Some(PathBuf::from(s));
    }
    Some(PathBuf::from(s))
}

pub fn position_json(p: LspPosition) -> Value {
    json!({"line": p.line, "character": p.character})
}

pub fn parse_position(v: &Value) -> Option<LspPosition> {
    Some(LspPosition {
        line: u32::try_from(v.get("line")?.as_u64()?).ok()?,
        character: u32::try_from(v.get("character")?.as_u64()?).ok()?,
    })
}

/// The protocol position of char index `pos`.
pub fn char_position(text: &Rope, pos: usize) -> Value {
    position_json(char_to_lsp(text, pos))
}

/// Converts a protocol `Range` to a char range in `text`. Returns `None`
/// when a line is past the end of the document.
pub fn range_to_chars(text: &Rope, range: &Value) -> Option<(usize, usize)> {
    let start = lsp_to_char(text, parse_position(range.get("start")?)?)?;
    let end = lsp_to_char(text, parse_position(range.get("end")?)?)?;
    Some((start.min(end), start.max(end)))
}

/// Converts a change set into `TextDocumentContentChangeEvent`s.
///
/// The protocol applies content changes in order, each against the result
/// of the previous one. [`ChangeSet::changes`] lists replacements in the old
/// document's coordinates, sorted and non-overlapping, so emitting them last
/// to first keeps every earlier position valid: a change never moves text
/// before it. Positions are computed on `old`, the text before `changes`.
///
/// Two line-ending details need care, both found by the randomized test in
/// `tests/client.rs`:
///
/// * a protocol position cannot point between the `\r` and `\n` of a CRLF
///   pair (it clamps to the end of the line, before the `\r`), so a change
///   that starts or ends there is widened to cover the whole pair and the
///   replacement text gets the covered half back;
/// * changes that touch are joined. Otherwise a later change that puts a
///   `\n` right after a `\r` would turn the boundary into the middle of a
///   new CRLF pair before the earlier change is applied.
pub fn content_changes(old: &Rope, changes: &ChangeSet) -> Vec<Value> {
    let mid_crlf =
        |i: usize| i > 0 && i < old.len_chars() && old.char(i - 1) == '\r' && old.char(i) == '\n';
    let mut merged: Vec<(usize, usize, String)> = Vec::new();
    for c in changes.changes() {
        let mut text = c.text.unwrap_or_default();
        if let Some(prev) = merged.last_mut().filter(|p| p.1 == c.from) {
            prev.1 = c.to;
            prev.2.push_str(&text);
            continue;
        }
        let mut from = c.from;
        if mid_crlf(from) {
            from -= 1;
            text.insert(0, '\r');
        }
        merged.push((from, c.to, text));
    }
    for (_, to, text) in &mut merged {
        if mid_crlf(*to) {
            *to += 1;
            text.push('\n');
        }
    }
    merged
        .into_iter()
        .rev()
        .map(|(from, to, text)| {
            json!({
                "range": {
                    "start": char_position(old, from),
                    "end": char_position(old, to),
                },
                "text": text,
            })
        })
        .collect()
}

/// The plain text of a hover result: `MarkupContent`, a `MarkedString`, or
/// an array of them. Code fences are dropped and their contents kept.
pub fn hover_text(result: &Value) -> Option<String> {
    let contents = result.get("contents")?;
    let mut parts = Vec::new();
    collect_marked(contents, &mut parts);
    let text = parts.join("\n\n");
    let mut lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect();
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn collect_marked(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|i| collect_marked(i, out)),
        Value::Object(o) => {
            if let Some(Value::String(s)) = o.get("value") {
                out.push(s.clone());
            }
        }
        _ => {}
    }
}

/// A location from a `Location` or `LocationLink`: the target URI and range.
pub fn location(v: &Value) -> Option<(String, Value)> {
    if let Some(uri) = v.get("targetUri").and_then(Value::as_str) {
        let range = v
            .get("targetSelectionRange")
            .or_else(|| v.get("targetRange"))?;
        return Some((uri.to_string(), range.clone()));
    }
    let uri = v.get("uri")?.as_str()?;
    Some((uri.to_string(), v.get("range")?.clone()))
}

/// Locations from a definition or references result: null, one location,
/// or an array of `Location`s or `LocationLink`s.
pub fn locations(result: &Value) -> Vec<(String, Value)> {
    match result {
        Value::Array(items) => items.iter().filter_map(location).collect(),
        Value::Object(_) => location(result).into_iter().collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_core::Change;

    #[test]
    fn uri_round_trip_with_spaces_and_unicode() {
        let p = Path::new("/tmp/a dir/ünï#.rs");
        let uri = path_to_uri(p);
        assert_eq!(uri, "file:///tmp/a%20dir/%C3%BCn%C3%AF%23.rs");
        assert_eq!(uri_to_path(&uri).as_deref(), Some(p));
        assert_eq!(
            uri_to_path("file://localhost/x/y").as_deref(),
            Some(Path::new("/x/y"))
        );
        assert_eq!(uri_to_path("untitled:1"), None);
        assert_eq!(uri_to_path("file:///bad%zz"), None);
    }

    #[test]
    fn positions_are_utf16_with_astral_chars() {
        // U+1F600 is one char, two UTF-16 units, four UTF-8 bytes.
        let text = Rope::from_str("a😀b\n😀😀x\n");
        assert_eq!(char_position(&text, 2), json!({"line": 0, "character": 3}));
        assert_eq!(char_position(&text, 6), json!({"line": 1, "character": 4}));
        let r = json!({"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5}});
        assert_eq!(range_to_chars(&text, &r), Some((5, 7)));
    }

    #[test]
    fn content_changes_run_last_to_first_in_old_coordinates() {
        let old = Rope::from_str("😀ab\ncd\n");
        let cs = ChangeSet::from_changes(
            old.len_chars(),
            [Change::insert(1, "X"), Change::replace(4, 6, "Y😀")],
        )
        .unwrap();
        let out = content_changes(&old, &cs);
        assert_eq!(out.len(), 2);
        // The later change comes first.
        assert_eq!(out[0]["range"]["start"], json!({"line": 1, "character": 0}));
        assert_eq!(out[0]["range"]["end"], json!({"line": 1, "character": 2}));
        assert_eq!(out[0]["text"], json!("Y😀"));
        assert_eq!(out[1]["range"]["start"], json!({"line": 0, "character": 2}));
        assert_eq!(out[1]["text"], json!("X"));
    }

    #[test]
    fn changes_inside_crlf_cover_the_whole_pair() {
        let old = Rope::from_str("a\r\nb");
        // Delete only the \n: the protocol cannot express that position.
        let cs = ChangeSet::from_changes(4, [Change::delete(2, 3)]).unwrap();
        let out = content_changes(&old, &cs);
        assert_eq!(out[0]["range"]["start"], json!({"line": 0, "character": 1}));
        assert_eq!(out[0]["range"]["end"], json!({"line": 1, "character": 0}));
        assert_eq!(out[0]["text"], json!("\r"));
        // Insert between \r and \n.
        let cs = ChangeSet::from_changes(4, [Change::insert(2, "x")]).unwrap();
        assert_eq!(content_changes(&old, &cs)[0]["text"], json!("\rx\n"));
        // Two changes meeting inside the pair become one.
        let cs =
            ChangeSet::from_changes(4, [Change::replace(0, 2, "P"), Change::replace(2, 4, "Q")])
                .unwrap();
        let out = content_changes(&old, &cs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["range"]["start"], json!({"line": 0, "character": 0}));
        assert_eq!(out[0]["range"]["end"], json!({"line": 1, "character": 1}));
        assert_eq!(out[0]["text"], json!("PQ"));
    }

    #[test]
    fn hover_markdown_becomes_plain_lines() {
        let h =
            json!({"contents": {"kind": "markdown", "value": "```rust\nfn f()\n```\n\nDoes f."}});
        assert_eq!(hover_text(&h).as_deref(), Some("fn f()\n\nDoes f."));
        let h = json!({"contents": ["plain", {"language": "rust", "value": "fn g()"}]});
        assert_eq!(hover_text(&h).as_deref(), Some("plain\n\nfn g()"));
        assert_eq!(hover_text(&json!({"contents": ""})), None);
    }

    #[test]
    fn locations_accept_links_and_single_objects() {
        let r = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}});
        let loc = json!({"uri": "file:///a", "range": r});
        let link = json!({"targetUri": "file:///b", "targetRange": r, "targetSelectionRange": r});
        assert_eq!(locations(&loc).len(), 1);
        assert_eq!(locations(&json!([loc, link]))[1].0, "file:///b");
        assert!(locations(&Value::Null).is_empty());
    }
}
