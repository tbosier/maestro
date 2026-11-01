//! Signature help replies: the signatures of the call around the cursor
//! and which parameter the cursor is in.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub label: String,
    /// The active parameter as a char range in `label`, if the server named
    /// one and it could be found.
    pub active: Option<(usize, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Help {
    pub signatures: Vec<Signature>,
    /// Index into `signatures`, always in range.
    pub active: usize,
}

/// Parses a `SignatureHelp` result. `None` for `null` or no signatures.
pub fn parse(result: &Value) -> Option<Help> {
    let sigs = result.get("signatures")?.as_array()?;
    let help_param = result.get("activeParameter");
    let signatures: Vec<Signature> = sigs
        .iter()
        .filter_map(|s| parse_signature(s, help_param))
        .collect();
    if signatures.is_empty() {
        return None;
    }
    let active = result
        .get("activeSignature")
        .and_then(Value::as_u64)
        .map_or(0, |i| i as usize);
    // Out of range falls back to the first, as the protocol says.
    let active = if active < signatures.len() { active } else { 0 };
    Some(Help { signatures, active })
}

fn parse_signature(s: &Value, help_param: Option<&Value>) -> Option<Signature> {
    let label = s.get("label")?.as_str()?.to_string();
    let params = s
        .get("parameters")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    // The signature's own `activeParameter` wins over the reply's. An
    // omitted index means the first parameter; `null` or an index past the
    // end means none is marked (typing a third argument to a two-parameter
    // function should not mark the first).
    let index = match s.get("activeParameter").or(help_param) {
        None => Some(0),
        Some(v) => v.as_u64().map(|i| i as usize),
    };
    let active = index
        .and_then(|i| params.get(i))
        .and_then(|p| param_range(&label, p.get("label")?));
    Some(Signature { label, active })
}

/// The char range of a parameter label in the signature label: either
/// `[start, end]` UTF-16 offsets or a substring, looked for after the first
/// `(` so a parameter named like the function is not found in its name.
fn param_range(label: &str, param: &Value) -> Option<(usize, usize)> {
    match param {
        Value::Array(pair) => {
            let start = utf16_to_char(label, pair.first()?.as_u64()? as usize)?;
            let end = utf16_to_char(label, pair.get(1)?.as_u64()? as usize)?;
            (start <= end).then_some((start, end))
        }
        Value::String(name) if !name.is_empty() => {
            let from = label.find('(').map_or(0, |i| i + 1);
            let at = label[from..].find(name.as_str())? + from;
            let start = label[..at].chars().count();
            Some((start, start + name.chars().count()))
        }
        _ => None,
    }
}

/// The char index of UTF-16 offset `units` in `s`, or `None` if it is past
/// the end or inside a surrogate pair.
fn utf16_to_char(s: &str, units: usize) -> Option<usize> {
    let mut seen = 0;
    for (i, c) in s.chars().enumerate() {
        if seen == units {
            return Some(i);
        }
        if seen > units {
            return None;
        }
        seen += c.len_utf16();
    }
    (seen == units).then(|| s.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn offsets_and_substrings_mark_the_active_parameter() {
        let help = parse(&json!({
            "signatures": [{
                "label": "fn f(😀: u8, b: u8)",
                // 😀 is two UTF-16 units: `😀: u8` is units 5..11.
                "parameters": [{"label": [5, 11]}, {"label": [13, 18]}],
            }],
            "activeParameter": 1,
        }))
        .unwrap();
        assert_eq!(help.active, 0);
        assert_eq!(help.signatures[0].active, Some((12, 17)));

        let help = parse(&json!({
            "signatures": [
                {"label": "x"},
                {"label": "f(f: u8, g: u8)", "parameters": [{"label": "f: u8"}, {"label": "g: u8"}],
                 "activeParameter": 0},
            ],
            "activeSignature": 1,
            "activeParameter": 1,
        }))
        .unwrap();
        assert_eq!(help.active, 1);
        // The signature's own index wins, and `f` is found after `(`.
        assert_eq!(help.signatures[1].active, Some((2, 7)));
    }

    #[test]
    fn missing_or_out_of_range_parameters() {
        let sig = |extra: Value| {
            let mut s = json!({"label": "f(a, b)", "parameters": [{"label": "a"}, {"label": "b"}]});
            if let (Value::Object(s), Value::Object(e)) = (&mut s, extra) {
                s.extend(e);
            }
            parse(&json!({"signatures": [s]})).unwrap().signatures[0].active
        };
        assert_eq!(sig(json!({})), Some((2, 3)), "omitted means the first");
        assert_eq!(sig(json!({"activeParameter": 5})), None);
        assert_eq!(sig(json!({"activeParameter": null})), None);
        assert_eq!(
            parse(&json!({"signatures": [{"label": "f()"}], "activeSignature": 9}))
                .unwrap()
                .active,
            0
        );
        assert!(parse(&Value::Null).is_none());
        assert!(parse(&json!({"signatures": []})).is_none());
        assert_eq!(utf16_to_char("a😀", 2), None, "inside a surrogate pair");
        assert_eq!(utf16_to_char("a😀", 3), Some(2));
    }
}
