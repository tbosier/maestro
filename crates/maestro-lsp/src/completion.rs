//! Completion replies: parsing `CompletionList` and `CompletionItem`, the
//! names of item kinds, and turning snippets into plain text.
//!
//! Ranges stay protocol JSON here; the editor converts them to char
//! indices against the text the request was made for.

use serde_json::Value;

/// Where an item's text goes.
#[derive(Debug, Clone, PartialEq)]
pub enum ItemEdit {
    /// Replace `range` (a protocol `Range`) with `text`. For an
    /// `InsertReplaceEdit` this is the insert range.
    Range { range: Value, text: String },
    /// Replace the identifier before the cursor with `text` (the item's
    /// `insertText`, or its label when there is none).
    Word(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub label: String,
    /// `CompletionItemKind`, 1 to 25.
    pub kind: Option<u64>,
    pub detail: Option<String>,
    pub filter_text: Option<String>,
    pub sort_text: Option<String>,
    pub preselect: bool,
    pub edit: ItemEdit,
    /// `insertTextFormat` 2: the text is a snippet.
    pub snippet: bool,
    /// `additionalTextEdits`: (protocol `Range`, new text).
    pub additional: Vec<(Value, String)>,
}

impl Item {
    /// The text filtering and ranking compare against.
    pub fn filter_text(&self) -> &str {
        self.filter_text.as_deref().unwrap_or(&self.label)
    }

    /// The text sorting falls back to.
    pub fn sort_text(&self) -> &str {
        self.sort_text.as_deref().unwrap_or(&self.label)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct List {
    pub items: Vec<Item>,
    /// `isIncomplete`: typing more must ask the server again.
    pub incomplete: bool,
}

/// Parses a completion result: `null`, `CompletionItem[]` or
/// `CompletionList`. Items without a label are skipped.
pub fn parse_list(result: &Value) -> List {
    let (items, incomplete) = match result {
        Value::Array(items) => (items.as_slice(), false),
        Value::Object(o) => (
            o.get("items")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice),
            o.get("isIncomplete")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
        _ => (&[][..], false),
    };
    List {
        items: items.iter().filter_map(parse_item).collect(),
        incomplete,
    }
}

fn string(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Parses one `CompletionItem`.
pub fn parse_item(v: &Value) -> Option<Item> {
    let label = string(v, "label")?;
    let edit = match v.get("textEdit") {
        Some(te) if te.get("insert").is_some() => ItemEdit::Range {
            range: te["insert"].clone(),
            text: string(te, "newText")?,
        },
        Some(te) if te.get("range").is_some() => ItemEdit::Range {
            range: te["range"].clone(),
            text: string(te, "newText")?,
        },
        _ => ItemEdit::Word(string(v, "insertText").unwrap_or_else(|| label.clone())),
    };
    let additional = v
        .get("additionalTextEdits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| Some((e.get("range")?.clone(), string(e, "newText")?)))
        .collect();
    Some(Item {
        kind: v.get("kind").and_then(Value::as_u64),
        detail: string(v, "detail").filter(|d| !d.is_empty()),
        filter_text: string(v, "filterText"),
        sort_text: string(v, "sortText"),
        preselect: v.get("preselect").and_then(Value::as_bool).unwrap_or(false),
        snippet: v.get("insertTextFormat").and_then(Value::as_u64) == Some(2),
        edit,
        additional,
        label,
    })
}

/// The word shown for a `CompletionItemKind`, so the kind never depends on
/// color or an icon.
pub fn kind_label(kind: u64) -> &'static str {
    match kind {
        1 => "text",
        2 => "method",
        3 => "function",
        4 => "constructor",
        5 => "field",
        6 => "variable",
        7 => "class",
        8 => "interface",
        9 => "module",
        10 => "property",
        11 => "unit",
        12 => "value",
        13 => "enum",
        14 => "keyword",
        15 => "snippet",
        16 => "color",
        17 => "file",
        18 => "reference",
        19 => "folder",
        20 => "variant",
        21 => "constant",
        22 => "struct",
        23 => "event",
        24 => "operator",
        25 => "type param",
        _ => "",
    }
}

/// Turns a snippet into plain text. Tab stops (`$1`, `${1}`) disappear,
/// placeholders (`${1:text}`) keep their text, choices (`${1|a,b|}`) keep
/// the first choice, and variables (`$TM_FILENAME`, `${NAME:default}`)
/// become their default or nothing. Backslash escapes `$`, `}` and `\`.
///
/// Returns the text and the char offset of the first `$0` in it, if any.
pub fn snippet_to_text(snippet: &str) -> (String, Option<usize>) {
    let chars: Vec<char> = snippet.chars().collect();
    let mut out = String::new();
    let mut cursor = None;
    let mut i = 0;
    parse_snippet(&chars, &mut i, false, &mut out, &mut cursor);
    (out, cursor)
}

/// Parses until the end, or until an unescaped `}` when `nested`.
fn parse_snippet(
    s: &[char],
    i: &mut usize,
    nested: bool,
    out: &mut String,
    cursor: &mut Option<usize>,
) {
    while *i < s.len() {
        let c = s[*i];
        match c {
            '\\' if *i + 1 < s.len() && matches!(s[*i + 1], '$' | '}' | '\\') => {
                out.push(s[*i + 1]);
                *i += 2;
            }
            '}' if nested => {
                *i += 1;
                return;
            }
            '$' => parse_dollar(s, i, out, cursor),
            _ => {
                out.push(c);
                *i += 1;
            }
        }
    }
}

fn is_var_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_var_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Handles a `$` at `s[*i]`.
fn parse_dollar(s: &[char], i: &mut usize, out: &mut String, cursor: &mut Option<usize>) {
    let start = *i;
    *i += 1;
    let mark_zero = |digits: &str, out: &String, cursor: &mut Option<usize>| {
        if digits.parse::<u32>() == Ok(0) && cursor.is_none() {
            *cursor = Some(out.chars().count());
        }
    };
    // `$1`, `$0`
    if s.get(*i).is_some_and(char::is_ascii_digit) {
        let digits: String = s[*i..].iter().take_while(|c| c.is_ascii_digit()).collect();
        *i += digits.len();
        mark_zero(&digits, out, cursor);
        return;
    }
    // `$NAME`
    if s.get(*i).copied().is_some_and(is_var_start) {
        while s.get(*i).copied().is_some_and(is_var_char) {
            *i += 1;
        }
        return;
    }
    if s.get(*i) != Some(&'{') {
        // A lone `$` is text.
        out.push('$');
        return;
    }
    *i += 1;
    let name_start = *i;
    let numeric = s.get(*i).is_some_and(char::is_ascii_digit);
    if numeric {
        while s.get(*i).is_some_and(char::is_ascii_digit) {
            *i += 1;
        }
    } else if s.get(*i).copied().is_some_and(is_var_start) {
        while s.get(*i).copied().is_some_and(is_var_char) {
            *i += 1;
        }
    } else {
        // Not a snippet construct: keep it literally.
        *i = start + 1;
        out.push('$');
        return;
    }
    let name: String = s[name_start..*i].iter().collect();
    match s.get(*i) {
        Some('}') => {
            *i += 1;
            if numeric {
                mark_zero(&name, out, cursor);
            }
        }
        Some(':') => {
            *i += 1;
            if numeric {
                mark_zero(&name, out, cursor);
            }
            parse_snippet(s, i, true, out, cursor);
        }
        Some('|') if numeric => {
            *i += 1;
            mark_zero(&name, out, cursor);
            let mut first = true;
            while *i < s.len() {
                match s[*i] {
                    '\\' if *i + 1 < s.len() => {
                        if first {
                            out.push(s[*i + 1]);
                        }
                        *i += 2;
                    }
                    ',' => {
                        first = false;
                        *i += 1;
                    }
                    '|' if s.get(*i + 1) == Some(&'}') => {
                        *i += 2;
                        return;
                    }
                    c => {
                        if first {
                            out.push(c);
                        }
                        *i += 1;
                    }
                }
            }
        }
        // `${VAR/regex/format/}` and anything malformed: skip to the
        // closing brace and insert nothing.
        _ => {
            while *i < s.len() && s[*i] != '}' {
                *i += if s[*i] == '\\' { 2 } else { 1 };
            }
            *i = (*i + 1).min(s.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snippets_become_plain_text_with_the_final_stop() {
        assert_eq!(snippet_to_text("foo($0)"), ("foo()".into(), Some(4)));
        assert_eq!(
            snippet_to_text("configure(${1:value}, ${2:\\$x})$0;"),
            ("configure(value, $x);".into(), Some(20))
        );
        assert_eq!(snippet_to_text("a${1}b$2c"), ("abc".into(), None));
        assert_eq!(
            snippet_to_text("${1:outer ${2:inner}} end"),
            ("outer inner end".into(), None)
        );
        assert_eq!(
            snippet_to_text("${1|one,two|} \\} \\\\"),
            ("one } \\".into(), None)
        );
        assert_eq!(
            snippet_to_text("$TM_FILENAME ${NAME:dflt} ${0:x}"),
            (" dflt x".into(), Some(6))
        );
        assert_eq!(
            snippet_to_text("cost $ 5 ${ x"),
            ("cost $ 5 ${ x".into(), None)
        );
        // Non-ASCII text before $0 counts chars, not bytes.
        assert_eq!(snippet_to_text("日本😀($0)"), ("日本😀()".into(), Some(4)));
        // A regex transform inserts nothing.
        assert_eq!(
            snippet_to_text("${TM_FILENAME/(.*)/$1/}x"),
            ("x".into(), None)
        );
    }

    #[test]
    fn lists_items_and_edits_parse() {
        let r = json!({"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 3}});
        let r2 = json!({"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 5}});
        let list = parse_list(&json!({
            "isIncomplete": true,
            "items": [
                {"label": "a", "textEdit": {"range": r, "newText": "aa"}, "kind": 3},
                {"label": "b", "textEdit": {"insert": r, "replace": r2, "newText": "bb"}},
                {"label": "c", "insertText": "cc", "insertTextFormat": 2},
                {"label": "d", "additionalTextEdits": [{"range": r, "newText": "x"}]},
                {"kind": 3},
            ],
        }));
        assert!(list.incomplete);
        assert_eq!(list.items.len(), 4);
        assert_eq!(
            list.items[0].edit,
            ItemEdit::Range {
                range: r.clone(),
                text: "aa".into()
            }
        );
        assert_eq!(
            list.items[1].edit,
            ItemEdit::Range {
                range: r.clone(),
                text: "bb".into()
            },
            "the insert range of an InsertReplaceEdit"
        );
        assert_eq!(list.items[2].edit, ItemEdit::Word("cc".into()));
        assert!(list.items[2].snippet);
        assert_eq!(list.items[3].edit, ItemEdit::Word("d".into()));
        assert_eq!(list.items[3].additional, vec![(r, "x".to_string())]);
        assert_eq!(kind_label(3), "function");
        let plain = parse_list(&json!([{"label": "z"}]));
        assert!(!plain.incomplete);
        assert_eq!(plain.items.len(), 1);
        assert!(parse_list(&Value::Null).items.is_empty());
    }
}
