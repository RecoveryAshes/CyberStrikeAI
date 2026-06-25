use anyhow::{Context, Result};
use serde_json::json;

use crate::event_protocol::{RuntimeCommand, RuntimeEvent};

pub mod pb {
    tonic::include_proto!("cyberstrike.agent_runtime.v1");
}

pub fn command_from_proto(command: pb::RuntimeCommand) -> Result<RuntimeCommand> {
    if !command.raw_json.trim().is_empty() {
        return serde_json::from_str(&command.raw_json).context("decode runtime command raw json");
    }

    let message = if command.r#type == "approval_response" {
        command.approval_message
    } else {
        command.message
    };
    let mut value = serde_json::json!({
        "type": command.r#type,
        "command_id": command.command_id,
        "conversation_id": command.conversation_id,
        "runtime_session_id": command.runtime_session_id,
        "message": message,
        "reason": command.reason,
        "continue_after": command.continue_after,
        "request_id": command.request_id,
        "decision": command.decision,
    });
    if !command.context_json.trim().is_empty() {
        value["context"] = serde_json::from_str(&command.context_json)
            .context("decode runtime command context json")?;
    }
    serde_json::from_value(value).context("decode runtime command from proto")
}

pub fn event_to_proto(event: &RuntimeEvent) -> Result<pb::RuntimeEvent> {
    let raw = serde_json::to_string(event).context("encode runtime event raw json")?;
    let value = serde_json::to_value(event).context("encode runtime event value")?;
    let event_type = value
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let items = value
        .get("items")
        .and_then(|v| v.as_array())
        .map(|values| {
            values
                .iter()
                .map(|item| pb::PlanItem {
                    id: string_field(item, "id"),
                    step: string_field(item, "step"),
                    status: string_field(item, "status"),
                    priority: string_field(item, "priority"),
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(pb::RuntimeEvent {
        r#type: event_type.clone(),
        command_id: string_field(&value, "command_id"),
        conversation_id: string_field(&value, "conversation_id"),
        runtime_session_id: string_field(&value, "runtime_session_id"),
        turn_id: string_field(&value, "turn_id"),
        delta: string_field(&value, "delta"),
        accumulated: string_field(&value, "accumulated"),
        response: string_field(&value, "response"),
        reason: string_field(&value, "reason"),
        message: string_field(&value, "message"),
        items,
        tool_call_id: string_field(&value, "tool_call_id"),
        tool_name: string_field(&value, "tool_name"),
        arguments_json: json_field(&value, "arguments")?,
        result: string_field(&value, "result"),
        error: string_field(&value, "error"),
        request_id: string_field(&value, "request_id"),
        permission: string_field(&value, "permission"),
        decision: string_field(&value, "decision"),
        summary: string_field(&value, "summary"),
        task_id: string_field(&value, "task_id"),
        strategy: string_field(&value, "strategy"),
        input_message_count: int_field(&value, "input_message_count"),
        input_chars: int_field(&value, "input_chars"),
        replacement_message_count: int_field(&value, "replacement_message_count"),
        artifact_path: string_field(&value, "artifact_path"),
        runtime_event_type: event_type.clone(),
        runtime_trace_json: runtime_trace_json(&value, &event_type)?,
        payload_json: payload_json(&value)?,
        occurred_at: now_unix_string(),
        sequence: String::new(),
        assistant_message_id: assistant_message_id(&value),
        event_id: String::new(),
        raw_json: raw,
    })
}

fn string_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn int_field(value: &serde_json::Value, key: &str) -> i64 {
    value.get(key).and_then(|v| v.as_i64()).unwrap_or_default()
}

fn json_field(value: &serde_json::Value, key: &str) -> Result<String> {
    let Some(field) = value.get(key) else {
        return Ok(String::new());
    };
    serde_json::to_string(field).context("encode runtime event json field")
}

fn payload_json(value: &serde_json::Value) -> Result<String> {
    serde_json::to_string(value).context("encode runtime event payload json")
}

fn runtime_trace_json(value: &serde_json::Value, event_type: &str) -> Result<String> {
    let mut trace = serde_json::Map::new();
    trace.insert(
        "schema".to_string(),
        json!("cyberstrike.agent_runtime.trace.v1"),
    );
    trace.insert("event".to_string(), json!(event_type));
    copy_string(value, &mut trace, "conversation_id", "conversationId");
    copy_string(value, &mut trace, "runtime_session_id", "runtimeSessionId");
    copy_string(value, &mut trace, "turn_id", "turnId");
    copy_string(value, &mut trace, "message", "message");
    copy_string(value, &mut trace, "delta", "delta");
    copy_string(value, &mut trace, "accumulated", "accumulated");
    copy_string(value, &mut trace, "response", "response");
    copy_string(value, &mut trace, "reason", "reason");
    copy_string(value, &mut trace, "summary", "summary");
    copy_string(value, &mut trace, "request_id", "requestId");
    copy_string(value, &mut trace, "permission", "permission");
    copy_string(value, &mut trace, "decision", "decision");
    if let Some(items) = value.get("items") {
        trace.insert("plan".to_string(), items.clone());
        trace.insert("items".to_string(), items.clone());
    }
    if value.get("tool_call_id").is_some()
        || value.get("tool_name").is_some()
        || value.get("arguments").is_some()
        || value.get("result").is_some()
        || value.get("error").is_some()
    {
        let mut tool = serde_json::Map::new();
        copy_string(value, &mut tool, "tool_call_id", "callId");
        copy_string(value, &mut tool, "tool_name", "name");
        if let Some(arguments) = value.get("arguments") {
            tool.insert("arguments".to_string(), arguments.clone());
        }
        copy_string(value, &mut tool, "result", "result");
        copy_string(value, &mut tool, "error", "error");
        trace.insert("tool".to_string(), serde_json::Value::Object(tool));
    }
    serde_json::to_string(&serde_json::Value::Object(trace)).context("encode runtime trace json")
}

fn copy_string(
    from: &serde_json::Value,
    to: &mut serde_json::Map<String, serde_json::Value>,
    source_key: &str,
    target_key: &str,
) {
    if let Some(value) = from.get(source_key).and_then(|v| v.as_str()) {
        if !value.trim().is_empty() {
            to.insert(target_key.to_string(), json!(value));
        }
    }
}

fn assistant_message_id(value: &serde_json::Value) -> String {
    value
        .get("assistant_message_id")
        .or_else(|| value.get("assistantMessageId"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn now_unix_string() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_from_proto_prefers_raw_json() {
        let command = pb::RuntimeCommand {
            raw_json: json!({
                "type": "start_turn",
                "command_id": "cmd-1",
                "conversation_id": "conv-1",
                "runtime_session_id": "session-1",
                "message": "hello",
                "context": {"max_steps": 3}
            })
            .to_string(),
            ..Default::default()
        };
        let command = command_from_proto(command).expect("decode command");
        match command {
            RuntimeCommand::StartTurn {
                command_id,
                conversation_id,
                context,
                ..
            } => {
                assert_eq!(command_id, "cmd-1");
                assert_eq!(conversation_id, "conv-1");
                assert_eq!(context["max_steps"], json!(3));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn event_to_proto_preserves_frontend_fields() {
        let event = RuntimeEvent::ToolCallStarted {
            conversation_id: "conv-1".to_string(),
            runtime_session_id: "session-1".to_string(),
            turn_id: "turn-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "runtime_echo".to_string(),
            arguments: json!({"message": "hello"}),
        };
        let proto = event_to_proto(&event).expect("encode event");
        assert_eq!(proto.r#type, "tool_call_started");
        assert_eq!(proto.tool_call_id, "call-1");
        assert_eq!(proto.tool_name, "runtime_echo");
        assert_eq!(
            proto.arguments_json,
            json!({"message": "hello"}).to_string()
        );
        assert!(proto.raw_json.contains("tool_call_started"));
    }
}
