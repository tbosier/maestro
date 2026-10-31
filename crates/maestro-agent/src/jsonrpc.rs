//! JSON-RPC 2.0 messages framed as newline-delimited JSON, as ACP uses them
//! on stdio.

use std::io::{self, BufRead, Read, Write};

use serde_json::{Value, json};

pub(crate) const INVALID_REQUEST: i64 = -32600;
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
pub(crate) const INVALID_PARAMS: i64 = -32602;
pub(crate) const INTERNAL_ERROR: i64 = -32603;
pub(crate) const RESOURCE_NOT_FOUND: i64 = -32002;

/// The longest line accepted from the other side. A file the agent writes
/// travels in one line, so this is generous; a longer line is a protocol
/// failure rather than something to buffer without bound.
pub(crate) const MAX_LINE: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    fn from_value(v: &Value) -> Self {
        Self {
            code: v.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string(),
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// A classified incoming message.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Message {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        result: Result<Value, RpcError>,
    },
}

impl Message {
    /// Classifies a parsed JSON value. `None` for anything that is not a
    /// JSON-RPC 2.0 message: no `"jsonrpc": "2.0"`, a request id that is
    /// not a string or number, or a response with neither `result` nor
    /// `error`.
    pub(crate) fn parse(v: Value) -> Option<Self> {
        let Value::Object(mut map) = v else {
            return None;
        };
        if map.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return None;
        }
        let id = map.remove("id");
        match (map.remove("method"), id) {
            (Some(Value::String(method)), Some(id)) if id.is_string() || id.is_number() => {
                Some(Message::Request {
                    id,
                    method,
                    params: map.remove("params").unwrap_or(Value::Null),
                })
            }
            (Some(Value::String(method)), None) => Some(Message::Notification {
                method,
                params: map.remove("params").unwrap_or(Value::Null),
            }),
            (None, Some(id)) => {
                let result = match (map.remove("result"), map.remove("error")) {
                    (_, Some(e)) if e.is_object() => Err(RpcError::from_value(&e)),
                    (Some(r), None | Some(Value::Null)) => Ok(r),
                    _ => return None,
                };
                Some(Message::Response { id, result })
            }
            _ => None,
        }
    }
}

pub(crate) fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

pub(crate) fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

pub(crate) fn response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub(crate) fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Writes one serialized message as a single line and flushes. Text from
/// serde_json has no raw newline: it escapes control characters.
pub(crate) fn write_text(w: &mut impl Write, text: &str) -> io::Result<()> {
    w.write_all(text.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}

/// One line read from the other side.
#[derive(Debug)]
pub(crate) enum Line {
    /// A complete line, without its line ending. May be empty.
    Text(Vec<u8>),
    /// The stream ended.
    Eof,
    /// The line was longer than [`MAX_LINE`].
    TooLong,
}

pub(crate) fn read_line(r: &mut impl BufRead) -> io::Result<Line> {
    let mut buf = Vec::new();
    let n = r.take(MAX_LINE + 1).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(Line::Eof);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    } else if n as u64 > MAX_LINE {
        return Ok(Line::TooLong);
    }
    Ok(Line::Text(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_messages() {
        assert_eq!(
            Message::parse(request(3, "a", json!([1]))),
            Some(Message::Request {
                id: json!(3),
                method: "a".into(),
                params: json!([1])
            })
        );
        assert_eq!(
            Message::parse(notification("n", Value::Null)),
            Some(Message::Notification {
                method: "n".into(),
                params: Value::Null
            })
        );
        assert_eq!(
            Message::parse(response(json!("x"), json!(5))),
            Some(Message::Response {
                id: json!("x"),
                result: Ok(json!(5))
            })
        );
        assert!(matches!(
            Message::parse(error_response(json!(1), -32800, "cancelled")),
            Some(Message::Response {
                result: Err(RpcError { code: -32800, .. }),
                ..
            })
        ));
        assert_eq!(Message::parse(json!([1, 2])), None);
        assert_eq!(Message::parse(json!({"jsonrpc": "2.0"})), None);
    }

    #[test]
    fn rejects_invalid_envelopes() {
        // No version, or the wrong one.
        assert_eq!(
            Message::parse(json!({"id": 1, "method": "fs/read_text_file"})),
            None
        );
        assert_eq!(
            Message::parse(json!({"jsonrpc": "1.0", "id": 1, "result": 1})),
            None
        );
        // An id alone is not a response.
        assert_eq!(Message::parse(json!({"jsonrpc": "2.0", "id": 1})), None);
        assert_eq!(
            Message::parse(json!({"jsonrpc": "2.0", "id": 1, "error": null})),
            None
        );
        // Request ids are strings or numbers.
        assert_eq!(
            Message::parse(json!({"jsonrpc": "2.0", "id": null, "method": "x"})),
            None
        );
        assert_eq!(
            Message::parse(json!({"jsonrpc": "2.0", "id": [1], "method": "x"})),
            None
        );
        // A null result is still a result.
        assert_eq!(
            Message::parse(json!({"jsonrpc": "2.0", "id": 1, "result": null})),
            Some(Message::Response {
                id: json!(1),
                result: Ok(Value::Null)
            })
        );
    }

    #[test]
    fn reads_lines_and_rejects_overlong_ones() {
        let mut r = io::Cursor::new(b"{\"a\":1}\r\n\nlast".to_vec());
        assert!(matches!(read_line(&mut r).unwrap(), Line::Text(t) if t == b"{\"a\":1}"));
        assert!(matches!(read_line(&mut r).unwrap(), Line::Text(t) if t.is_empty()));
        assert!(matches!(read_line(&mut r).unwrap(), Line::Text(t) if t == b"last"));
        assert!(matches!(read_line(&mut r).unwrap(), Line::Eof));
        let long = vec![b'x'; MAX_LINE as usize + 10];
        let mut r = io::Cursor::new(long);
        assert!(matches!(read_line(&mut r).unwrap(), Line::TooLong));
    }

    #[test]
    fn written_lines_are_single_lines() {
        let mut out = Vec::new();
        write_text(&mut out, &json!({"text": "a\nb"}).to_string()).unwrap();
        assert_eq!(out.iter().filter(|&&b| b == b'\n').count(), 1);
        assert_eq!(out.last(), Some(&b'\n'));
    }
}
