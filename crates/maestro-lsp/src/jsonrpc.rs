//! JSON-RPC 2.0 messages as the Language Server Protocol uses them.

use serde_json::{Value, json};

/// Ids the client allocates for its own requests. Servers may use strings
/// for theirs; those are echoed back verbatim as [`Value`]s.
pub type RequestId = i64;

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const REQUEST_CANCELLED: i64 = -32800;
    pub const CONTENT_MODIFIED: i64 = -32801;

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
pub enum Message {
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
    /// Classifies a parsed JSON value. Returns `None` for anything that is
    /// not a JSON-RPC message.
    pub fn parse(v: Value) -> Option<Self> {
        let Value::Object(mut map) = v else {
            return None;
        };
        let id = map.remove("id");
        match (map.remove("method"), id) {
            (Some(Value::String(method)), Some(id)) => Some(Message::Request {
                id,
                method,
                params: map.remove("params").unwrap_or(Value::Null),
            }),
            (Some(Value::String(method)), None) => Some(Message::Notification {
                method,
                params: map.remove("params").unwrap_or(Value::Null),
            }),
            (None, Some(id)) => {
                let result = match map.remove("error") {
                    Some(e) if !e.is_null() => Err(RpcError::from_value(&e)),
                    _ => Ok(map.remove("result").unwrap_or(Value::Null)),
                };
                Some(Message::Response { id, result })
            }
            _ => None,
        }
    }
}

pub fn request(id: RequestId, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

pub fn response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
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
        let err = Message::parse(error_response(json!(1), -32800, "cancelled"));
        assert!(matches!(
            err,
            Some(Message::Response {
                result: Err(RpcError { code: -32800, .. }),
                ..
            })
        ));
        assert_eq!(Message::parse(json!([1, 2])), None);
        assert_eq!(Message::parse(json!({"jsonrpc": "2.0"})), None);
    }
}
