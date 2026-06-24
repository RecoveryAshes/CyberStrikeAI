use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeCommand {
    StartTurn {
        #[serde(default)]
        command_id: String,
        conversation_id: String,
        runtime_session_id: Option<String>,
        message: String,
        #[serde(default)]
        context: Map<String, Value>,
    },
    InterruptTurn {
        #[serde(default)]
        command_id: String,
        conversation_id: String,
        #[serde(default)]
        reason: String,
        #[serde(default)]
        continue_after: bool,
    },
    ApprovalResponse {
        #[serde(default)]
        command_id: String,
        #[serde(default)]
        conversation_id: String,
        #[serde(default)]
        runtime_session_id: Option<String>,
        request_id: String,
        decision: String,
        #[serde(default)]
        message: String,
        #[serde(default)]
        context: Map<String, Value>,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    SessionStarted {
        conversation_id: String,
        runtime_session_id: String,
    },
    TurnStarted {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
    },
    PlanUpdated {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        items: Vec<PlanEventItem>,
    },
    ReasoningDelta {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        delta: String,
    },
    AssistantProgressUpdate {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        message: String,
    },
    RuntimeStatusUpdate {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        message: String,
    },
    AssistantDelta {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        delta: String,
        accumulated: String,
    },
    ToolCallStarted {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        arguments: Value,
    },
    ToolCallDelta {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        tool_call_id: String,
        delta: String,
    },
    ToolCallCompleted {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        result: String,
    },
    ToolCallFailed {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        error: String,
    },
    ApprovalRequested {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        request_id: String,
        permission: String,
        tool_call_id: String,
        tool_name: String,
        arguments: Value,
        message: String,
    },
    ApprovalResolved {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        request_id: String,
        decision: String,
    },
    FollowUpStarted {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        reason: String,
    },
    CompactionStarted {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        task_id: String,
        strategy: String,
        input_message_count: usize,
        input_chars: usize,
    },
    CompactionCompleted {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        task_id: String,
        strategy: String,
        input_message_count: usize,
        input_chars: usize,
        replacement_message_count: usize,
        artifact_path: String,
        summary: String,
    },
    StopHookContinued {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        reason: String,
    },
    TurnCompleted {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        response: String,
    },
    TurnAborted {
        conversation_id: String,
        runtime_session_id: String,
        turn_id: String,
        reason: String,
    },
    RuntimeError {
        conversation_id: String,
        runtime_session_id: String,
        message: String,
    },
    CommandCompleted {
        command_id: String,
        conversation_id: String,
        runtime_session_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanEventItem {
    pub id: String,
    pub step: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
}

impl RuntimeEvent {
    pub fn runtime_error(
        conversation_id: impl Into<String>,
        runtime_session_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        RuntimeEvent::RuntimeError {
            conversation_id: conversation_id.into(),
            runtime_session_id: runtime_session_id.into(),
            message: message.into(),
        }
    }
}

pub fn new_id(prefix: &str) -> String {
    format!("{}_{}", prefix, Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::RuntimeEvent;

    #[test]
    fn serializes_assistant_progress_update_event() {
        let value = serde_json::to_value(RuntimeEvent::AssistantProgressUpdate {
            conversation_id: "conv-1".to_string(),
            runtime_session_id: "session-1".to_string(),
            turn_id: "turn-1".to_string(),
            message: "checking current process list".to_string(),
        })
        .expect("serialize assistant progress update");

        assert_eq!(value["type"], "assistant_progress_update");
        assert_eq!(value["conversation_id"], "conv-1");
        assert_eq!(value["runtime_session_id"], "session-1");
        assert_eq!(value["turn_id"], "turn-1");
        assert_eq!(value["message"], "checking current process list");
    }

    #[test]
    fn serializes_runtime_status_update_event() {
        let value = serde_json::to_value(RuntimeEvent::RuntimeStatusUpdate {
            conversation_id: "conv-1".to_string(),
            runtime_session_id: "session-1".to_string(),
            turn_id: "turn-1".to_string(),
            message: "requesting model sample".to_string(),
        })
        .expect("serialize runtime status update");

        assert_eq!(value["type"], "runtime_status_update");
        assert_eq!(value["conversation_id"], "conv-1");
        assert_eq!(value["runtime_session_id"], "session-1");
        assert_eq!(value["turn_id"], "turn-1");
        assert_eq!(value["message"], "requesting model sample");
    }
}
