use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::model_stream::{ChatMessage, ModelStream};
use crate::plan_store::PlanStore;

pub const COMPACTION_SUMMARY_SYSTEM_PROMPT: &str = "You are compacting an agent session. Summarize the prior conversation, tool calls, results, decisions, active plan state, and unresolved user intent. Preserve concrete file paths, commands, MCP tool names, skill/resource references, errors, approvals, and next actions. Do not add new facts.";

#[derive(Debug, Clone)]
pub struct CompactionTask {
    pub id: String,
    pub strategy: String,
    pub input_message_count: usize,
    pub input_chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionArtifact {
    pub task_id: String,
    pub strategy: String,
    pub input_message_count: usize,
    pub input_chars: usize,
    pub input_messages: Vec<ChatMessage>,
    #[serde(default)]
    pub summary_attempts: Vec<CompactionSummaryAttempt>,
    #[serde(default)]
    pub summary_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_error: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub replacement_metadata: CompactionReplacementMetadata,
    pub replacement_messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CompactionSummaryAttempt {
    pub source: String,
    pub input_message_count: usize,
    pub input_chars: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CompactionReplacementMetadata {
    pub original_message_count: usize,
    pub original_chars: usize,
    pub preserved_system_messages: usize,
    pub kept_recent_messages: usize,
    pub replacement_message_count: usize,
}

#[derive(Debug, Clone)]
pub struct CompactionRuntime {
    enabled: bool,
    threshold_chars: usize,
    keep_recent_messages: usize,
    max_compactions_per_turn: usize,
    prompt_max_chars: usize,
    retry_limit: usize,
    compaction_count: usize,
}

#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub summary: String,
    pub messages: Vec<ChatMessage>,
    pub artifact: CompactionArtifact,
}

pub trait ModelCompactor {
    fn summarize(&self, messages: &[ChatMessage]) -> Result<String, String>;
}

impl ModelCompactor for ModelStream {
    fn summarize(&self, messages: &[ChatMessage]) -> Result<String, String> {
        ModelStream::summarize(self, messages).map_err(|err| err.to_string())
    }
}

impl Default for CompactionRuntime {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold_chars: 40_000,
            keep_recent_messages: 8,
            max_compactions_per_turn: 4,
            prompt_max_chars: 80_000,
            retry_limit: 1,
            compaction_count: 0,
        }
    }
}

impl CompactionRuntime {
    pub fn from_context(context: &Map<String, Value>) -> Self {
        let enabled = context
            .get("compaction_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let threshold_chars = context
            .get("compaction_threshold_chars")
            .or_else(|| context.get("compact_threshold_chars"))
            .and_then(Value::as_u64)
            .map(|v| v.max(1) as usize)
            .unwrap_or(40_000);
        let keep_recent_messages = context
            .get("compaction_keep_recent_messages")
            .or_else(|| context.get("compact_keep_recent_messages"))
            .and_then(Value::as_u64)
            .map(|v| v.max(1) as usize)
            .unwrap_or(8);
        let max_compactions_per_turn = context
            .get("compaction_max_per_turn")
            .or_else(|| context.get("compact_max_per_turn"))
            .and_then(Value::as_u64)
            .map(|v| v.max(1) as usize)
            .unwrap_or(4);
        let prompt_max_chars = context
            .get("compaction_prompt_max_chars")
            .or_else(|| context.get("compact_prompt_max_chars"))
            .and_then(Value::as_u64)
            .map(|v| v.max(1) as usize)
            .unwrap_or_else(|| threshold_chars.saturating_mul(2).max(80_000));
        let retry_limit = context
            .get("compaction_retry_limit")
            .or_else(|| context.get("compact_retry_limit"))
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(1);
        Self {
            enabled,
            threshold_chars,
            keep_recent_messages,
            max_compactions_per_turn,
            prompt_max_chars,
            retry_limit,
            compaction_count: 0,
        }
    }

    pub fn should_compact(&self, messages: &[ChatMessage]) -> bool {
        self.enabled
            && self.compaction_count < self.max_compactions_per_turn
            && estimate_chars(messages) >= self.threshold_chars
    }

    pub fn start_task(&self, turn_id: &str, messages: &[ChatMessage]) -> CompactionTask {
        CompactionTask {
            id: format!("compaction_{}_{}", turn_id, self.compaction_count + 1),
            strategy: "rollout_summary_with_recent_tail".to_string(),
            input_message_count: messages.len(),
            input_chars: estimate_chars(messages),
        }
    }

    pub fn compact_with_model(
        &mut self,
        task: &CompactionTask,
        messages: &[ChatMessage],
        plan: &PlanStore,
        compactor: &dyn ModelCompactor,
    ) -> CompactionResult {
        self.compaction_count = self.compaction_count.saturating_add(1);
        let fallback_summary = build_summary(messages, plan);
        let mut attempts = Vec::new();
        let mut prompt_messages = compaction_prompt_messages(messages, plan);
        if estimate_chars(&prompt_messages) > self.prompt_max_chars {
            prompt_messages = compaction_prompt_messages(
                &trim_messages_for_compaction_prompt(
                    messages,
                    self.keep_recent_messages,
                    self.prompt_max_chars,
                ),
                plan,
            );
        }
        let mut last_error = None;
        for attempt_index in 0..=self.retry_limit {
            let source = if attempt_index == 0 {
                "model".to_string()
            } else {
                "model_trimmed_retry".to_string()
            };
            let input_message_count = prompt_messages.len();
            let input_chars = estimate_chars(&prompt_messages);
            match compactor
                .summarize(&prompt_messages)
                .map(|summary| summary.trim().to_string())
            {
                Ok(summary) if !summary.is_empty() => {
                    attempts.push(CompactionSummaryAttempt {
                        source: source.clone(),
                        input_message_count,
                        input_chars,
                        error: None,
                    });
                    return self.compact_with_summary(
                        task, messages, summary, &source, last_error, attempts,
                    );
                }
                Ok(_) => {
                    let err = "model returned an empty compaction summary".to_string();
                    attempts.push(CompactionSummaryAttempt {
                        source,
                        input_message_count,
                        input_chars,
                        error: Some(err.clone()),
                    });
                    last_error = Some(err);
                }
                Err(err) => {
                    attempts.push(CompactionSummaryAttempt {
                        source,
                        input_message_count,
                        input_chars,
                        error: Some(err.clone()),
                    });
                    last_error = Some(err);
                }
            }
            prompt_messages = compaction_prompt_messages(
                &trim_messages_for_compaction_prompt(
                    messages,
                    self.keep_recent_messages,
                    self.prompt_max_chars / 2,
                ),
                plan,
            );
        }
        let summary_source = if last_error
            .as_deref()
            .is_some_and(|err| err.contains("empty compaction summary"))
        {
            "local_heuristic_empty_model_fallback"
        } else {
            "local_heuristic_error_fallback"
        };
        self.compact_with_summary(
            task,
            messages,
            fallback_summary,
            summary_source,
            last_error,
            attempts,
        )
    }

    fn compact_with_summary(
        &mut self,
        task: &CompactionTask,
        messages: &[ChatMessage],
        summary: String,
        summary_source: &str,
        summary_error: Option<String>,
        summary_attempts: Vec<CompactionSummaryAttempt>,
    ) -> CompactionResult {
        let mut compacted = Vec::new();
        if let Some(system) = messages.iter().find(|message| message.role == "system") {
            compacted.push(system.clone());
        }
        compacted.push(ChatMessage::system(format!(
            "COMPACTED_CONTEXT_SUMMARY:\n{}",
            summary
        )));

        let recent_start = messages.len().saturating_sub(self.keep_recent_messages);
        for message in messages.iter().skip(recent_start) {
            if message.role == "system"
                && message
                    .content
                    .as_deref()
                    .unwrap_or_default()
                    .starts_with("COMPACTED_CONTEXT_SUMMARY:")
            {
                continue;
            }
            if compacted
                .last()
                .is_some_and(|last| same_message_identity(last, message))
            {
                continue;
            }
            compacted.push(message.clone());
        }
        let artifact = CompactionArtifact {
            task_id: task.id.clone(),
            strategy: task.strategy.clone(),
            input_message_count: task.input_message_count,
            input_chars: task.input_chars,
            input_messages: messages.to_vec(),
            summary_attempts,
            summary_source: summary_source.to_string(),
            summary_error,
            summary: summary.clone(),
            replacement_metadata: CompactionReplacementMetadata {
                original_message_count: messages.len(),
                original_chars: estimate_chars(messages),
                preserved_system_messages: compacted
                    .iter()
                    .filter(|message| message.role == "system")
                    .count()
                    .saturating_sub(1),
                kept_recent_messages: messages.len().saturating_sub(recent_start),
                replacement_message_count: compacted.len(),
            },
            replacement_messages: compacted.clone(),
        };
        CompactionResult {
            summary,
            messages: compacted,
            artifact,
        }
    }
}

pub fn compaction_prompt_messages(messages: &[ChatMessage], plan: &PlanStore) -> Vec<ChatMessage> {
    vec![
        ChatMessage::system(COMPACTION_SUMMARY_SYSTEM_PROMPT),
        ChatMessage::user(format!(
            "Plan state:\n{}\n\nConversation transcript to compact:\n{}",
            render_plan_for_summary(plan),
            render_messages_for_summary(messages)
        )),
    ]
}

fn estimate_chars(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|message| {
            message
                .content
                .as_deref()
                .unwrap_or_default()
                .chars()
                .count()
                + message
                    .tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|call| {
                                call.function.name.chars().count()
                                    + call.function.arguments.chars().count()
                            })
                            .sum::<usize>()
                    })
                    .unwrap_or(0)
        })
        .sum()
}

fn build_summary(messages: &[ChatMessage], plan: &PlanStore) -> String {
    let mut lines = Vec::new();
    if !plan.event_items().is_empty() {
        lines.push("Plan state:".to_string());
        for item in plan.event_items() {
            lines.push(format!("- [{}] {} ({})", item.id, item.step, item.status));
        }
    }
    lines.push("Prior conversation/tool state:".to_string());
    for message in messages.iter().filter(|message| message.role != "system") {
        let content = message.content.as_deref().unwrap_or_default().trim();
        if !content.is_empty() {
            lines.push(format!(
                "- {}: {}",
                message.role,
                truncate(content, 500).replace('\n', " ")
            ));
        }
        if let Some(tool_calls) = &message.tool_calls {
            for call in tool_calls {
                lines.push(format!(
                    "- assistant tool call {}: {} {}",
                    call.id,
                    call.function.name,
                    truncate(&call.function.arguments, 300)
                ));
            }
        }
    }
    lines.join("\n")
}

fn render_plan_for_summary(plan: &PlanStore) -> String {
    let items = plan.event_items();
    if items.is_empty() {
        return "(no active plan items)".to_string();
    }
    items
        .iter()
        .map(|item| format!("- [{}] {} ({})", item.id, item.step, item.status))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_messages_for_summary(messages: &[ChatMessage]) -> String {
    let mut lines = Vec::new();
    for message in messages {
        if let Some(content) = message
            .content
            .as_deref()
            .filter(|content| !content.trim().is_empty())
        {
            lines.push(format!(
                "[{}] {}",
                message.role,
                truncate(content.trim(), 2000)
            ));
        }
        if let Some(tool_calls) = &message.tool_calls {
            for call in tool_calls {
                lines.push(format!(
                    "[assistant tool_call {}] {} {}",
                    call.id,
                    call.function.name,
                    truncate(&call.function.arguments, 1000)
                ));
            }
        }
    }
    lines.join("\n")
}

fn trim_messages_for_compaction_prompt(
    messages: &[ChatMessage],
    keep_recent_messages: usize,
    max_chars: usize,
) -> Vec<ChatMessage> {
    if estimate_chars(messages) <= max_chars {
        return messages.to_vec();
    }
    let mut system = messages
        .iter()
        .find(|message| message.role == "system")
        .cloned();
    let mut running_chars = system
        .as_ref()
        .map(|message| estimate_chars(std::slice::from_ref(message)))
        .unwrap_or(0);
    let mut tail = Vec::new();
    let tail_start = messages.len().saturating_sub(keep_recent_messages.max(1));
    for message in messages.iter().skip(tail_start).rev() {
        let message_chars = estimate_chars(std::slice::from_ref(message));
        if running_chars.saturating_add(message_chars) > max_chars && !tail.is_empty() {
            continue;
        }
        running_chars = running_chars.saturating_add(message_chars);
        tail.push(message.clone());
        if running_chars >= max_chars {
            break;
        }
    }
    tail.reverse();
    let mut trimmed = Vec::new();
    if let Some(system) = system.take() {
        trimmed.push(system);
    }
    trimmed.extend(tail);
    if trimmed.is_empty() {
        messages.iter().rev().take(1).cloned().collect()
    } else {
        trimmed
    }
}

fn same_message_identity(a: &ChatMessage, b: &ChatMessage) -> bool {
    a.role == b.role && a.content == b.content && a.tool_call_id == b.tool_call_id
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_store::{PlanItem, PlanStatus};
    use serde_json::{json, Map};

    #[test]
    fn compacts_messages_when_threshold_is_reached() {
        let mut context = Map::new();
        context.insert("compaction_enabled".to_string(), Value::Bool(true));
        context.insert("compaction_threshold_chars".to_string(), json!(10));
        context.insert("compaction_keep_recent_messages".to_string(), json!(1));
        context.insert("compaction_max_per_turn".to_string(), json!(1));
        let mut runtime = CompactionRuntime::from_context(&context);
        let mut plan = PlanStore::default();
        plan.update(vec![PlanItem {
            id: "p1".to_string(),
            step: "Do work".to_string(),
            status: PlanStatus::InProgress,
            priority: None,
        }])
        .unwrap();
        let messages = vec![
            ChatMessage::system("system prompt"),
            ChatMessage::user("a long user message"),
            ChatMessage::assistant(Some("assistant answer".to_string()), Vec::new()),
        ];
        assert!(runtime.should_compact(&messages));
        let task = runtime.start_task("turn-1", &messages);
        let compactor = StubCompactor {
            result: Ok(build_summary(&messages, &plan)),
        };
        let result = runtime.compact_with_model(&task, &messages, &plan, &compactor);
        assert!(result.summary.contains("Plan state"));
        assert_eq!(result.artifact.input_messages, messages);
        assert_eq!(result.artifact.replacement_messages, result.messages);
        assert_eq!(result.artifact.task_id, "compaction_turn-1_1");
        assert!(result.messages.iter().any(|message| message
            .content
            .as_deref()
            .unwrap_or_default()
            .contains("COMPACTED_CONTEXT_SUMMARY")));
        assert!(!runtime.should_compact(&result.messages));
    }

    #[test]
    fn can_compact_multiple_times_until_per_turn_limit() {
        let mut context = Map::new();
        context.insert("compaction_enabled".to_string(), Value::Bool(true));
        context.insert("compaction_threshold_chars".to_string(), json!(1));
        context.insert("compaction_keep_recent_messages".to_string(), json!(2));
        context.insert("compaction_max_per_turn".to_string(), json!(2));
        let mut runtime = CompactionRuntime::from_context(&context);
        let plan = PlanStore::default();
        let compactor = StubCompactor {
            result: Ok("summary".to_string()),
        };
        let messages = vec![
            ChatMessage::system("system prompt"),
            ChatMessage::user("first long message"),
            ChatMessage::assistant(Some("assistant answer".to_string()), Vec::new()),
        ];

        assert!(runtime.should_compact(&messages));
        let first = runtime.start_task("turn-repeat", &messages);
        let first_result = runtime.compact_with_model(&first, &messages, &plan, &compactor);
        assert_eq!(first_result.artifact.task_id, "compaction_turn-repeat_1");

        assert!(runtime.should_compact(&first_result.messages));
        let second = runtime.start_task("turn-repeat", &first_result.messages);
        let second_result =
            runtime.compact_with_model(&second, &first_result.messages, &plan, &compactor);
        assert_eq!(second_result.artifact.task_id, "compaction_turn-repeat_2");
        assert!(!runtime.should_compact(&second_result.messages));
    }

    #[test]
    fn uses_model_summary_when_available() {
        let mut context = Map::new();
        context.insert("compaction_enabled".to_string(), Value::Bool(true));
        let mut runtime = CompactionRuntime::from_context(&context);
        let messages = vec![
            ChatMessage::system("system prompt"),
            ChatMessage::user("hello"),
        ];
        let task = runtime.start_task("turn-model", &messages);
        let plan = PlanStore::default();
        let compactor = StubCompactor {
            result: Ok("model compacted summary".to_string()),
        };

        let result = runtime.compact_with_model(&task, &messages, &plan, &compactor);

        assert_eq!(result.summary, "model compacted summary");
        assert_eq!(result.artifact.summary_source, "model");
        assert!(result.artifact.summary_error.is_none());
        assert_eq!(result.artifact.summary_attempts.len(), 1);
        assert_eq!(result.artifact.summary_attempts[0].source, "model");
        assert_eq!(
            result
                .artifact
                .replacement_metadata
                .replacement_message_count,
            result.messages.len()
        );
        assert!(result.messages.iter().any(|message| message
            .content
            .as_deref()
            .unwrap_or_default()
            .contains("model compacted summary")));
    }

    #[test]
    fn falls_back_to_local_summary_when_model_compaction_fails() {
        let mut runtime = CompactionRuntime::default();
        let messages = vec![
            ChatMessage::system("system prompt"),
            ChatMessage::user("important detail"),
        ];
        let task = runtime.start_task("turn-fallback", &messages);
        let plan = PlanStore::default();
        let compactor = StubCompactor {
            result: Err("remote compaction unavailable".to_string()),
        };

        let result = runtime.compact_with_model(&task, &messages, &plan, &compactor);

        assert!(result.summary.contains("important detail"));
        assert_eq!(
            result.artifact.summary_source,
            "local_heuristic_error_fallback"
        );
        assert_eq!(
            result.artifact.summary_error.as_deref(),
            Some("remote compaction unavailable")
        );
        assert!(!result.artifact.summary_attempts.is_empty());
        assert_eq!(
            result.artifact.summary_attempts[0].error.as_deref(),
            Some("remote compaction unavailable")
        );
    }

    struct StubCompactor {
        result: Result<String, String>,
    }

    impl ModelCompactor for StubCompactor {
        fn summarize(&self, _messages: &[ChatMessage]) -> Result<String, String> {
            self.result.clone()
        }
    }
}
