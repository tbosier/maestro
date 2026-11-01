//! Workspace edits: parsing a `WorkspaceEdit` (from rename, a code action
//! or `workspace/applyEdit`) or a `TextEdit[]` (from formatting) into one
//! list of text edits per document.
//!
//! Ranges stay protocol JSON here. The editor converts and checks them
//! against the text they apply to, and refuses the whole edit if any part
//! is wrong.

use serde_json::Value;

/// One `TextEdit` (or `AnnotatedTextEdit`; the annotation is ignored).
#[derive(Debug, Clone, PartialEq)]
pub struct TextEdit {
    /// A protocol `Range`.
    pub range: Value,
    pub new_text: String,
}

/// The edits for one document.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentEdit {
    pub uri: String,
    /// The document version the edits were computed for, from a
    /// `TextDocumentEdit`. `None` for the `changes` form and for a `null`
    /// version, which means the file as it is on disk.
    pub version: Option<i64>,
    pub edits: Vec<TextEdit>,
}

/// Parses a `TextEdit[]`. `null` is no edits.
pub fn parse_text_edits(v: &Value) -> Result<Vec<TextEdit>, String> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items.iter().map(parse_text_edit).collect(),
        _ => Err("expected a list of text edits".into()),
    }
}

fn parse_text_edit(v: &Value) -> Result<TextEdit, String> {
    let range = v
        .get("range")
        .filter(|r| r.is_object())
        .ok_or("a text edit has no range")?;
    let new_text = v
        .get("newText")
        .and_then(Value::as_str)
        .ok_or("a text edit has no newText")?;
    Ok(TextEdit {
        range: range.clone(),
        new_text: new_text.to_string(),
    })
}

/// Parses a `WorkspaceEdit`. `documentChanges` wins over `changes` when
/// both are present, as the protocol says.
///
/// Refused, so that nothing of the edit is applied:
///
/// * resource operations (create, rename or delete a file);
/// * the same document twice in `documentChanges`. The protocol applies
///   such entries in sequence, each against the result of the last, and
///   the versions a server would have to give the later ones cannot be
///   checked;
/// * anything malformed.
pub fn parse_workspace_edit(v: &Value) -> Result<Vec<DocumentEdit>, String> {
    if !v.is_object() {
        return Err("the edit is not an object".into());
    }
    let mut out: Vec<DocumentEdit> = Vec::new();
    match v.get("documentChanges") {
        Some(Value::Array(changes)) => {
            for change in changes {
                if let Some(kind) = change.get("kind").and_then(Value::as_str) {
                    return Err(format!(
                        "the edit would {kind} a file, which Maestro does not support"
                    ));
                }
                let doc = change
                    .get("textDocument")
                    .ok_or("a document change has no textDocument")?;
                let uri = doc
                    .get("uri")
                    .and_then(Value::as_str)
                    .ok_or("a document change has no uri")?;
                let version = match doc.get("version") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(v.as_i64().ok_or("a document version is not a number")?),
                };
                let edits = match change.get("edits") {
                    Some(Value::Array(items)) => items
                        .iter()
                        .map(parse_text_edit)
                        .collect::<Result<_, _>>()?,
                    _ => return Err("a document change has no edits".into()),
                };
                if out.iter().any(|d| d.uri == uri) {
                    return Err(format!(
                        "the edit changes {uri} more than once, which Maestro does not support"
                    ));
                }
                out.push(DocumentEdit {
                    uri: uri.to_string(),
                    version,
                    edits,
                });
            }
        }
        Some(Value::Null) | None => {
            if let Some(changes) = v.get("changes").filter(|c| !c.is_null()) {
                let map = changes.as_object().ok_or("changes is not an object")?;
                for (uri, edits) in map {
                    out.push(DocumentEdit {
                        uri: uri.clone(),
                        version: None,
                        edits: parse_text_edits(edits)?,
                    });
                }
            }
        }
        Some(_) => return Err("documentChanges is not a list".into()),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn range(a: u32, b: u32) -> Value {
        json!({"start": {"line": 0, "character": a}, "end": {"line": 0, "character": b}})
    }

    #[test]
    fn changes_and_document_changes_parse() {
        let edit = json!({"changes": {
            "file:///a.rs": [{"range": range(0, 1), "newText": "x"}],
            "file:///b.rs": [],
        }});
        let docs = parse_workspace_edit(&edit).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].uri, "file:///a.rs");
        assert_eq!(docs[0].version, None);
        assert_eq!(docs[0].edits[0].new_text, "x");

        // documentChanges wins; null versions mean the file on disk;
        // annotated edits keep their range and text.
        let edit = json!({
            "changes": {"file:///ignored.rs": []},
            "documentChanges": [
                {"textDocument": {"uri": "file:///a.rs", "version": 4},
                 "edits": [{"range": range(1, 2), "newText": "y", "annotationId": "n"}]},
                {"textDocument": {"uri": "file:///b.rs", "version": null}, "edits": []},
            ],
        });
        let docs = parse_workspace_edit(&edit).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].version, Some(4));
        assert_eq!(docs[0].edits[0].range, range(1, 2));
        assert_eq!(docs[1].version, None);
        assert!(parse_workspace_edit(&json!({})).unwrap().is_empty());
    }

    #[test]
    fn resource_operations_duplicates_and_junk_are_refused() {
        let create = json!({"documentChanges": [
            {"textDocument": {"uri": "file:///a.rs", "version": 1}, "edits": []},
            {"kind": "create", "uri": "file:///new.rs"},
        ]});
        let e = parse_workspace_edit(&create).unwrap_err();
        assert!(e.contains("create a file"), "{e}");
        let twice = json!({"documentChanges": [
            {"textDocument": {"uri": "file:///a.rs", "version": 1}, "edits": []},
            {"textDocument": {"uri": "file:///a.rs", "version": 2}, "edits": []},
        ]});
        assert!(
            parse_workspace_edit(&twice)
                .unwrap_err()
                .contains("more than once")
        );
        let no_text = json!({"changes": {"file:///a.rs": [{"range": range(0, 1)}]}});
        assert!(parse_workspace_edit(&no_text).is_err());
        assert!(parse_workspace_edit(&json!([1])).is_err());
        assert!(parse_text_edits(&json!({"a": 1})).is_err());
        assert!(parse_text_edits(&Value::Null).unwrap().is_empty());
    }
}
