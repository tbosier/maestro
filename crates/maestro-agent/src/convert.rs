//! Protocol messages to [`AcpEvent`]s and back, using the schema crate's
//! types.

use std::path::PathBuf;

use agent_client_protocol_schema::v1::{
    ContentBlock, PermissionOption, RequestPermissionRequest, SessionNotification, SessionUpdate,
    ToolCallLocation,
};
use serde::Serialize;
use serde_json::Value;

use crate::{AcpEvent, PermissionChoice, PermissionRequest, TranscriptEntry};

/// The snake_case name serde gives a unit enum value.
fn name_of(v: impl Serialize) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        _ => "unknown".to_string(),
    }
}

fn text_of(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(t) => t.text.clone(),
        ContentBlock::ResourceLink(link) => link.uri.clone(),
        ContentBlock::Image(_) => "[image]".to_string(),
        ContentBlock::Audio(_) => "[audio]".to_string(),
        ContentBlock::Resource(_) => "[resource]".to_string(),
        _ => "[unsupported content]".to_string(),
    }
}

fn paths(locations: &[ToolCallLocation]) -> Vec<PathBuf> {
    locations.iter().map(|l| l.path.clone()).collect()
}

/// The `sessionUpdate` kinds this client turns into events. Other kinds
/// (commands, modes, usage, and anything newer than this client) are
/// ignored.
const HANDLED: &[&str] = &[
    "user_message_chunk",
    "agent_message_chunk",
    "agent_thought_chunk",
    "tool_call",
    "tool_call_update",
    "plan",
];

/// Converts `session/update` params. `Ok(None)` for updates that are
/// ignored; `Err` when an update of a handled kind does not parse.
pub(crate) fn session_update(params: Value) -> Result<Option<(String, AcpEvent)>, String> {
    let kind = params
        .get("update")
        .and_then(|u| u.get("sessionUpdate"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if !HANDLED.contains(&kind.as_str()) {
        return Ok(None);
    }
    let n: SessionNotification = serde_json::from_value(params)
        .map_err(|e| format!("malformed {kind} session update: {e}"))?;
    let session = n.session_id.0.to_string();
    let event = match n.update {
        SessionUpdate::UserMessageChunk(c) => {
            AcpEvent::Update(TranscriptEntry::User(text_of(&c.content)))
        }
        SessionUpdate::AgentMessageChunk(c) => {
            AcpEvent::Update(TranscriptEntry::Agent(text_of(&c.content)))
        }
        SessionUpdate::AgentThoughtChunk(c) => {
            AcpEvent::Update(TranscriptEntry::Thought(text_of(&c.content)))
        }
        SessionUpdate::ToolCall(t) => AcpEvent::Update(TranscriptEntry::Tool {
            id: t.tool_call_id.0.to_string(),
            title: t.title,
            kind: name_of(t.kind),
            status: name_of(t.status),
            locations: paths(&t.locations),
        }),
        SessionUpdate::ToolCallUpdate(u) => AcpEvent::ToolUpdate {
            id: u.tool_call_id.0.to_string(),
            status: u.fields.status.map(name_of),
            title: u.fields.title,
            locations: u.fields.locations.as_deref().map(paths),
        },
        SessionUpdate::Plan(p) => AcpEvent::Update(TranscriptEntry::Plan(
            p.entries
                .into_iter()
                .map(|e| (e.content, name_of(e.status)))
                .collect(),
        )),
        _ => return Ok(None),
    };
    Ok(Some((session, event)))
}

/// Converts `session/request_permission` params, returning the session id
/// and the request.
pub(crate) fn permission_request(
    request_id: Value,
    params: Value,
) -> Result<(String, PermissionRequest), String> {
    let r: RequestPermissionRequest =
        serde_json::from_value(params).map_err(|e| format!("malformed permission request: {e}"))?;
    let tool_id = r.tool_call.tool_call_id.0.to_string();
    let tool_title = r
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| tool_id.clone());
    let options = r.options.iter().map(choice).collect();
    Ok((
        r.session_id.0.to_string(),
        PermissionRequest {
            request_id,
            tool_id,
            tool_title,
            options,
        },
    ))
}

fn choice(o: &PermissionOption) -> PermissionChoice {
    PermissionChoice {
        id: o.option_id.0.to_string(),
        label: o.name.clone(),
        kind: name_of(o.kind),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_chunks_tools_and_plans() {
        let chunk = |kind: &str| json!({"sessionId": "s", "update": {"sessionUpdate": kind, "content": {"type": "text", "text": "hi"}}});
        assert_eq!(
            session_update(chunk("agent_message_chunk")).unwrap(),
            Some((
                "s".to_string(),
                AcpEvent::Update(TranscriptEntry::Agent("hi".into()))
            ))
        );
        assert!(matches!(
            session_update(chunk("agent_thought_chunk")).unwrap(),
            Some((_, AcpEvent::Update(TranscriptEntry::Thought(_))))
        ));
        assert!(matches!(
            session_update(chunk("user_message_chunk")).unwrap(),
            Some((_, AcpEvent::Update(TranscriptEntry::User(_))))
        ));
        let tool = json!({"sessionId": "s", "update": {
            "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit a.rs",
            "kind": "edit", "status": "in_progress",
            "locations": [{"path": "/w/a.rs", "line": 3}]}});
        assert_eq!(
            session_update(tool).unwrap().unwrap().1,
            AcpEvent::Update(TranscriptEntry::Tool {
                id: "t1".into(),
                title: "Edit a.rs".into(),
                kind: "edit".into(),
                status: "in_progress".into(),
                locations: vec![PathBuf::from("/w/a.rs")],
            })
        );
        let update = json!({"sessionId": "s", "update": {
            "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"}});
        assert_eq!(
            session_update(update).unwrap().unwrap().1,
            AcpEvent::ToolUpdate {
                id: "t1".into(),
                status: Some("completed".into()),
                title: None,
                locations: None
            }
        );
        let plan = json!({"sessionId": "s", "update": {"sessionUpdate": "plan", "entries": [
            {"content": "a", "priority": "high", "status": "pending"},
            {"content": "b", "priority": "low", "status": "completed"}]}});
        assert_eq!(
            session_update(plan).unwrap().unwrap().1,
            AcpEvent::Update(TranscriptEntry::Plan(vec![
                ("a".into(), "pending".into()),
                ("b".into(), "completed".into())
            ]))
        );
    }

    #[test]
    fn ignores_other_kinds_and_reports_malformed_ones() {
        let modes = json!({"sessionId": "s", "update": {"sessionUpdate": "current_mode_update", "currentModeId": "x"}});
        assert_eq!(session_update(modes).unwrap(), None);
        let cmds = json!({"sessionId": "s", "update": {"sessionUpdate": "available_commands_update", "availableCommands": []}});
        assert_eq!(session_update(cmds).unwrap(), None);
        let future = json!({"sessionId": "s", "update": {"sessionUpdate": "something_new"}});
        assert_eq!(session_update(future).unwrap(), None);
        let bad = json!({"sessionId": "s", "update": {"sessionUpdate": "tool_call"}});
        assert!(session_update(bad).is_err());
    }

    #[test]
    fn maps_permission_requests() {
        let params = json!({"sessionId": "s", "toolCall": {"toolCallId": "t1", "title": "Write x"},
            "options": [{"optionId": "ok", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "no", "name": "Reject", "kind": "reject_always"}]});
        let (session, req) = permission_request(json!(7), params).unwrap();
        assert_eq!(session, "s");
        assert_eq!(req.request_id, json!(7));
        assert_eq!(req.tool_id, "t1");
        assert_eq!(req.tool_title, "Write x");
        assert_eq!(req.options[1].kind, "reject_always");
        assert!(permission_request(json!(1), json!({"sessionId": "s"})).is_err());
    }
}
