//! Wire types for the Maestro automation API.
//!
//! Messages are JSON-RPC 2.0 objects, one per line (newline-delimited JSON),
//! over stdin/stdout (`maestro --headless`) or a Unix socket
//! (`maestro --listen`). This crate holds only the shapes and constants on
//! the wire. It has no dependency on the editor, so an external client can
//! depend on it alone.
//!
//! A session starts with `initialize`, where the client states the protocol
//! version it speaks. The server refuses a different major version with
//! [`ErrorCode::UnsupportedProtocolVersion`] and refuses every other method
//! before a successful `initialize` with [`ErrorCode::NotInitialized`].
//!
//! Positions are explicit about their encoding. Every request that carries
//! positions also carries an [`Encoding`]: `"char"` positions are Unicode
//! scalar value offsets into the document, `"lsp"` positions are a zero-based
//! line and a UTF-16 code unit offset within it, as in the Language Server
//! Protocol.
//!
//! See `docs/automation.md` for every method with real examples.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol version this crate describes, as `MAJOR.MINOR`.
pub const PROTOCOL_VERSION: &str = "1.0";
pub const PROTOCOL_MAJOR: u64 = 1;
pub const PROTOCOL_MINOR: u64 = 0;
/// The `jsonrpc` member of every message.
pub const JSONRPC_VERSION: &str = "2.0";

/// Method names.
pub mod methods {
    pub const INITIALIZE: &str = "initialize";
    pub const EDITOR_SNAPSHOT: &str = "editor.snapshot";
    pub const EDITOR_EXECUTE: &str = "editor.execute";
    pub const EDITOR_QUIT: &str = "editor.quit";
    pub const DOCUMENT_OPEN: &str = "document.open";
    pub const DOCUMENT_TEXT: &str = "document.text";
    pub const DOCUMENT_SAVE: &str = "document.save";
    pub const DOCUMENT_APPLY_EDIT: &str = "document.apply_edit";
    pub const DOCUMENT_HISTORY: &str = "document.history";
    pub const SELECTIONS_GET: &str = "selections.get";
    pub const SELECTIONS_SET: &str = "selections.set";
    pub const COMMAND_RUN: &str = "command.run";
    pub const COMMAND_LIST: &str = "command.list";
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";

    /// Every request method, in the order `initialize` reports them.
    pub const ALL: &[&str] = &[
        INITIALIZE,
        EDITOR_SNAPSHOT,
        EDITOR_EXECUTE,
        EDITOR_QUIT,
        DOCUMENT_OPEN,
        DOCUMENT_TEXT,
        DOCUMENT_SAVE,
        DOCUMENT_APPLY_EDIT,
        DOCUMENT_HISTORY,
        SELECTIONS_GET,
        SELECTIONS_SET,
        COMMAND_RUN,
        COMMAND_LIST,
        EVENTS_SUBSCRIBE,
    ];
}

/// Notification (server to client) method names.
pub mod notifications {
    pub const DOCUMENT_CHANGED: &str = "document.changed";
    pub const EDITOR_STATUS: &str = "editor.status";

    pub const ALL: &[&str] = &[DOCUMENT_CHANGED, EDITOR_STATUS];
}

// ----- envelopes -------------------------------------------------------------

/// A request id. JSON-RPC allows numbers and strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

/// A client request. Without an `id` it is a JSON-RPC notification: the
/// server runs it and sends no response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl Request {
    pub fn new(id: impl Into<RequestId>, method: &str, params: impl Serialize) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(id.into()),
            method: method.to_string(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        }
    }
}

impl From<i64> for RequestId {
    fn from(n: i64) -> Self {
        RequestId::Number(n)
    }
}

impl From<&str> for RequestId {
    fn from(s: &str) -> Self {
        RequestId::String(s.to_string())
    }
}

/// Either a result or an error, never both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    #[serde(rename = "result")]
    Result(Value),
    #[serde(rename = "error")]
    Error(RpcError),
}

/// The server's reply to a request with an id. `id` is `null` only when the
/// request could not be parsed far enough to read its id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Option<RequestId>,
    #[serde(flatten)]
    pub outcome: Outcome,
}

impl Response {
    pub fn ok(id: Option<RequestId>, result: impl Serialize) -> Self {
        let outcome = match serde_json::to_value(result) {
            Ok(v) => Outcome::Result(v),
            Err(e) => Outcome::Error(RpcError::new(ErrorCode::Internal, e.to_string())),
        };
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            outcome,
        }
    }

    pub fn err(id: Option<RequestId>, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            outcome: Outcome::Error(error),
        }
    }

    pub fn result(&self) -> Option<&Value> {
        match &self.outcome {
            Outcome::Result(v) => Some(v),
            Outcome::Error(_) => None,
        }
    }

    pub fn error(&self) -> Option<&RpcError> {
        match &self.outcome {
            Outcome::Result(_) => None,
            Outcome::Error(e) => Some(e),
        }
    }
}

/// A server-to-client notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Notification {
    pub fn new(method: &str, params: impl Serialize) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.to_string(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        }
    }
}

/// Anything the server sends.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    Response(Response),
    Notification(Notification),
}

impl ServerMessage {
    /// Parses one line from the server. A message with a `method` member is a
    /// notification; anything else must be a response.
    pub fn parse(line: &str) -> Result<Self, serde_json::Error> {
        let v: Value = serde_json::from_str(line)?;
        if v.get("method").is_some() {
            serde_json::from_value(v).map(ServerMessage::Notification)
        } else {
            serde_json::from_value(v).map(ServerMessage::Response)
        }
    }
}

// ----- errors ----------------------------------------------------------------

/// Error codes. The first five are JSON-RPC 2.0's own; the rest are
/// Maestro's, in the range JSON-RPC reserves for implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// The line was not valid JSON.
    ParseError,
    /// Valid JSON but not a request object.
    InvalidRequest,
    MethodNotFound,
    /// Params are missing, malformed, out of bounds or overlapping.
    InvalidParams,
    Internal,
    /// A method other than `initialize` arrived before `initialize`.
    NotInitialized,
    /// `initialize` named a major version this server does not speak.
    UnsupportedProtocolVersion,
    /// An edit's `base_version` is not the document's current version.
    /// `data` holds `{"expected": n, "current": m}`. Nothing was changed.
    StaleVersion,
    /// No document, view, command or file by that name.
    NotFound,
    /// The document is read-only (binary, undecodable or unwritable file).
    ReadOnly,
    /// The operating system refused a file operation.
    PermissionDenied,
    /// The file changed on disk since it was read; save with `force`.
    Conflict,
    /// Quitting would discard unsaved changes; pass `force`.
    UnsavedChanges,
    /// The command ran and reported an error in the status line, or needs
    /// interactive input the API cannot supply. The message is that text.
    CommandFailed,
    /// Another I/O failure.
    Io,
}

impl ErrorCode {
    pub const fn code(self) -> i64 {
        match self {
            ErrorCode::ParseError => -32700,
            ErrorCode::InvalidRequest => -32600,
            ErrorCode::MethodNotFound => -32601,
            ErrorCode::InvalidParams => -32602,
            ErrorCode::Internal => -32603,
            ErrorCode::NotInitialized => -32002,
            ErrorCode::UnsupportedProtocolVersion => -32003,
            ErrorCode::StaleVersion => -32010,
            ErrorCode::NotFound => -32011,
            ErrorCode::ReadOnly => -32012,
            ErrorCode::PermissionDenied => -32013,
            ErrorCode::Conflict => -32014,
            ErrorCode::UnsavedChanges => -32015,
            ErrorCode::CommandFailed => -32016,
            ErrorCode::Io => -32017,
        }
    }

    pub const ALL: &'static [ErrorCode] = &[
        ErrorCode::ParseError,
        ErrorCode::InvalidRequest,
        ErrorCode::MethodNotFound,
        ErrorCode::InvalidParams,
        ErrorCode::Internal,
        ErrorCode::NotInitialized,
        ErrorCode::UnsupportedProtocolVersion,
        ErrorCode::StaleVersion,
        ErrorCode::NotFound,
        ErrorCode::ReadOnly,
        ErrorCode::PermissionDenied,
        ErrorCode::Conflict,
        ErrorCode::UnsavedChanges,
        ErrorCode::CommandFailed,
        ErrorCode::Io,
    ];

    pub fn from_code(code: i64) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.code() == code)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.code(),
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: impl Serialize) -> Self {
        self.data = serde_json::to_value(data).ok();
        self
    }

    pub fn kind(&self) -> Option<ErrorCode> {
        ErrorCode::from_code(self.code)
    }
}

/// `data` of a [`ErrorCode::StaleVersion`] error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleData {
    pub expected: u64,
    pub current: u64,
}

/// `data` of a [`ErrorCode::UnsupportedProtocolVersion`] error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsupportedVersionData {
    pub requested: String,
    pub supported: String,
}

/// Parses `MAJOR.MINOR` (a bare `MAJOR` is accepted too).
pub fn parse_version(v: &str) -> Option<(u64, u64)> {
    let mut parts = v.trim().splitn(2, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = match parts.next() {
        Some(m) => m.parse().ok()?,
        None => 0,
    };
    Some((major, minor))
}

// ----- shared shapes ---------------------------------------------------------

pub type DocId = u64;
pub type ViewId = u64;

/// How positions in a request are counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    /// Unicode scalar value (Rust `char`) offset from the start of the document.
    Char,
    /// Zero-based line and UTF-16 code unit offset within the line.
    Lsp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

/// A position in either encoding. The request's [`Encoding`] says which one
/// is expected; a mismatch is refused with [`ErrorCode::InvalidParams`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Position {
    Char(u64),
    Lsp(LspPosition),
}

/// A half-open range `start..end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextRange {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Normal,
    Insert,
    Select,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub severity: Severity,
    pub message: String,
}

/// One selection range. Maestro selections are inclusive, as in Kakoune: a
/// range covers every grapheme from `min(anchor, head)` through the grapheme
/// that starts at `max(anchor, head)`, so a collapsed range still covers the
/// character under the cursor. Both ends are given as char offsets and as
/// LSP positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionRange {
    pub anchor: u64,
    pub head: u64,
    pub anchor_lsp: LspPosition,
    pub head_lsp: LspPosition,
}

// ----- initialize ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeParams {
    /// The version the client speaks, `MAJOR.MINOR`.
    pub protocol_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeResult {
    /// The version the server will speak on this connection.
    pub protocol_version: String,
    pub server_name: String,
    pub server_version: String,
    /// Request methods this server implements.
    pub methods: Vec<String>,
    /// Notifications this server can send after `events.subscribe`.
    pub notifications: Vec<String>,
    /// Feature flags, for example `"apply_edit.lsp_positions"`.
    pub capabilities: Vec<String>,
}

// ----- editor ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferInfo {
    pub id: DocId,
    /// Absolute path, or `null` for a scratch buffer.
    pub path: Option<String>,
    /// The name the editor shows (the path, or `[scratch]`).
    pub name: String,
    pub version: u64,
    pub modified: bool,
    /// Why the buffer is read-only, or `null` if it is writable.
    pub read_only: Option<String>,
    /// Lines as the editor shows them: a trailing newline does not start an
    /// extra line.
    pub line_count: u64,
    pub char_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewInfo {
    pub id: ViewId,
    pub doc: DocId,
    pub selections: Vec<SelectionRange>,
    /// Index of the primary range in `selections`.
    pub primary: u64,
    /// First visible line, zero-based.
    pub top_line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotResult {
    pub buffers: Vec<BufferInfo>,
    pub views: Vec<ViewInfo>,
    pub focused_view: ViewId,
    pub mode: Mode,
    pub status: Option<Status>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteParams {
    /// A `:` command line without the colon, for example `"w"` or `"%s/a/b/"`.
    pub line: String,
}

/// Result of `editor.execute` and `command.run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResult {
    pub mode: Mode,
    /// The status line after the command; it was cleared before running.
    pub status: Option<Status>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuitParams {
    /// Quit even if buffers have unsaved changes.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuitResult {}

// ----- documents -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentOpenParams {
    /// Absolute, or relative to the editor's working directory. `~` expands.
    pub path: String,
    /// Also show the document in the focused view. Default false, so an
    /// agent opening a file does not move the user's view.
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentOpenResult {
    pub doc: DocId,
    pub path: String,
    pub version: u64,
    pub read_only: Option<String>,
    /// True if the document was already open.
    pub already_open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentTextParams {
    pub doc: DocId,
    /// Required when `range` is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<Encoding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<TextRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentTextResult {
    pub doc: DocId,
    pub version: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSaveParams {
    pub doc: DocId,
    /// Overwrite even if the file changed on disk since it was read.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSaveResult {
    pub doc: DocId,
    pub path: String,
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub range: TextRange,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyEditParams {
    pub doc: DocId,
    /// The document version the edits were computed against.
    pub base_version: u64,
    pub encoding: Encoding,
    /// Ranges refer to the document at `base_version`. They may be in any
    /// order but may not overlap; touching ranges are fine.
    pub edits: Vec<TextEdit>,
    /// Who made the edit, for example `"agent:refactor-bot"`. Recorded as
    /// given.
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyEditResult {
    pub doc: DocId,
    pub base_version: u64,
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocParams {
    pub doc: DocId,
}

/// One edit applied through `document.apply_edit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditRecord {
    pub actor: String,
    pub description: Option<String>,
    pub base_version: u64,
    pub new_version: u64,
    /// Milliseconds since the Unix epoch, from the editor's clock.
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryResult {
    pub doc: DocId,
    /// Oldest first. Only edits made through the API are listed.
    pub edits: Vec<EditRecord>,
}

// ----- selections ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewParams {
    pub view: ViewId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeSpec {
    pub anchor: Position,
    pub head: Position,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionsSetParams {
    pub view: ViewId,
    pub encoding: Encoding,
    pub ranges: Vec<RangeSpec>,
    /// Index into `ranges`; defaults to the last range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionsResult {
    pub view: ViewId,
    pub doc: DocId,
    pub version: u64,
    /// After the editor normalized them: sorted, merged where they overlap,
    /// snapped to grapheme boundaries.
    pub selections: Vec<SelectionRange>,
    pub primary: u64,
}

// ----- commands --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRunParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandKind {
    /// A command from the registry, bindable to keys.
    Command,
    /// A `:` command.
    Typed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyBinding {
    pub mode: Mode,
    /// Space-separated keys, for example `"g g"` or `"C-w v"`.
    pub keys: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    pub kind: CommandKind,
    pub doc: String,
    pub aliases: Vec<String>,
    pub keys: Vec<KeyBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandListResult {
    pub commands: Vec<CommandInfo>,
}

// ----- events ----------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscribeResult {
    pub notifications: Vec<String>,
}

/// `document.changed`. Versions can skip: changes made between two checks
/// are reported once, with the latest version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentChanged {
    pub doc: DocId,
    pub version: u64,
    /// The actor of the API edit that produced this version, or `null` when
    /// the change did not come through `document.apply_edit` (typing,
    /// commands, undo, reload).
    pub actor: Option<String>,
}

/// `editor.status`: the status line changed. `status` is `null` when it was
/// cleared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusChanged {
    pub status: Option<Status>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let s = serde_json::to_string(value).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn request_round_trips_and_omits_missing_id() {
        let req = Request::new(
            7,
            methods::DOCUMENT_TEXT,
            DocumentTextParams {
                doc: 3,
                encoding: None,
                range: None,
            },
        );
        assert_eq!(round_trip(&req), req);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(
            v,
            json!({"jsonrpc": "2.0", "id": 7, "method": "document.text", "params": {"doc": 3}})
        );

        let note: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"editor.snapshot"}"#).unwrap();
        assert_eq!(note.id, None);
        assert_eq!(note.params, Value::Null);
        let s = serde_json::to_string(&note).unwrap();
        assert!(!s.contains("\"id\""), "{s}");
    }

    #[test]
    fn string_ids_survive() {
        let req: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":"abc","method":"x"}"#).unwrap();
        assert_eq!(req.id, Some(RequestId::String("abc".into())));
    }

    #[test]
    fn response_has_exactly_one_of_result_and_error() {
        let ok = Response::ok(Some(RequestId::Number(1)), json!({"a": 1}));
        let v = serde_json::to_value(&ok).unwrap();
        assert_eq!(v, json!({"jsonrpc": "2.0", "id": 1, "result": {"a": 1}}));
        assert_eq!(round_trip(&ok), ok);

        let err = Response::err(
            None,
            RpcError::new(ErrorCode::StaleVersion, "stale").with_data(StaleData {
                expected: 1,
                current: 2,
            }),
        );
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(
            v,
            json!({"jsonrpc": "2.0", "id": null, "error": {
                "code": -32010, "message": "stale", "data": {"expected": 1, "current": 2}
            }})
        );
        let back = round_trip(&err);
        assert_eq!(back, err);
        assert_eq!(back.error().unwrap().kind(), Some(ErrorCode::StaleVersion));
    }

    #[test]
    fn server_messages_are_told_apart() {
        let n = Notification::new(
            notifications::DOCUMENT_CHANGED,
            DocumentChanged {
                doc: 1,
                version: 4,
                actor: Some("agent".into()),
            },
        );
        let line = serde_json::to_string(&n).unwrap();
        assert_eq!(
            ServerMessage::parse(&line).unwrap(),
            ServerMessage::Notification(n)
        );
        let r = Response::ok(Some(RequestId::Number(2)), QuitResult {});
        let line = serde_json::to_string(&r).unwrap();
        assert_eq!(
            ServerMessage::parse(&line).unwrap(),
            ServerMessage::Response(r)
        );
    }

    #[test]
    fn positions_in_both_encodings() {
        let edit: TextEdit = serde_json::from_value(json!({
            "range": {"start": 3, "end": {"line": 1, "character": 2}},
            "text": "x"
        }))
        .unwrap();
        assert_eq!(edit.range.start, Position::Char(3));
        assert_eq!(
            edit.range.end,
            Position::Lsp(LspPosition {
                line: 1,
                character: 2
            })
        );
        assert_eq!(round_trip(&edit), edit);

        let p: ApplyEditParams = serde_json::from_value(json!({
            "doc": 1, "base_version": 0, "encoding": "lsp", "actor": "a",
            "edits": []
        }))
        .unwrap();
        assert_eq!(p.encoding, Encoding::Lsp);
        assert_eq!(p.description, None);
        assert_eq!(round_trip(&p), p);
    }

    #[test]
    fn typed_results_round_trip() {
        let snap = SnapshotResult {
            buffers: vec![BufferInfo {
                id: 1,
                path: Some("/tmp/a".into()),
                name: "/tmp/a".into(),
                version: 2,
                modified: true,
                read_only: None,
                line_count: 3,
                char_count: 10,
            }],
            views: vec![ViewInfo {
                id: 2,
                doc: 1,
                selections: vec![SelectionRange {
                    anchor: 0,
                    head: 1,
                    anchor_lsp: LspPosition {
                        line: 0,
                        character: 0,
                    },
                    head_lsp: LspPosition {
                        line: 0,
                        character: 1,
                    },
                }],
                primary: 0,
                top_line: 0,
            }],
            focused_view: 2,
            mode: Mode::Normal,
            status: Some(Status {
                severity: Severity::Warning,
                message: "m".into(),
            }),
        };
        assert_eq!(round_trip(&snap), snap);
        let v = serde_json::to_value(&snap).unwrap();
        assert_eq!(v["mode"], "normal");
        assert_eq!(v["status"]["severity"], "warning");

        let hist = HistoryResult {
            doc: 1,
            edits: vec![EditRecord {
                actor: "a".into(),
                description: None,
                base_version: 0,
                new_version: 1,
                timestamp_ms: 5,
            }],
        };
        assert_eq!(round_trip(&hist), hist);

        let list = CommandListResult {
            commands: vec![CommandInfo {
                name: "write".into(),
                kind: CommandKind::Typed,
                doc: "Save".into(),
                aliases: vec!["w".into()],
                keys: vec![KeyBinding {
                    mode: Mode::Normal,
                    keys: "C-s".into(),
                }],
            }],
        };
        assert_eq!(round_trip(&list), list);
    }

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version("1.0"), Some((1, 0)));
        assert_eq!(parse_version("2"), Some((2, 0)));
        assert_eq!(parse_version("1.x"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(
            parse_version(PROTOCOL_VERSION),
            Some((PROTOCOL_MAJOR, PROTOCOL_MINOR))
        );
    }

    #[test]
    fn error_codes_are_unique_and_reversible() {
        let mut seen = std::collections::HashSet::new();
        for c in ErrorCode::ALL {
            assert!(seen.insert(c.code()), "duplicate code {}", c.code());
            assert_eq!(ErrorCode::from_code(c.code()), Some(*c));
        }
    }
}
