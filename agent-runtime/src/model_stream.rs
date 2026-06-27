use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub provider: String,
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub reasoning_effort: String,
    pub request_timeout_seconds: u64,
}

impl ModelConfig {
    pub fn from_context(context: &Map<String, Value>) -> Option<Self> {
        let api_key = context_string(context, "openai_api_key");
        let model = context_string(context, "openai_model");
        if api_key.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self {
            provider: context_string(context, "openai_provider"),
            api_key,
            base_url: context_string(context, "openai_base_url"),
            model,
            reasoning_effort: normalize_reasoning_effort(&context_string(
                context,
                "openai_reasoning_effort",
            )),
            request_timeout_seconds: context_u64(context, "tool_timeout_seconds").unwrap_or(120),
        })
    }

    fn base_url_effective(&self) -> String {
        let base = self.base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            "https://api.openai.com/v1".to_string()
        } else {
            base.to_string()
        }
    }
}

fn context_string(context: &Map<String, Value>, key: &str) -> String {
    context
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn context_u64(context: &Map<String, Value>, key: &str) -> Option<u64> {
    let value = context.get(key)?;
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct ModelStream {
    config: Option<ModelConfig>,
    client: reqwest::blocking::Client,
    tools: Value,
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("agent runtime only supports OpenAI-compatible providers for now, got {0}")]
    UnsupportedProvider(String),
    #[error("call OpenAI-compatible chat completions: {0}")]
    Http(#[from] reqwest::Error),
    #[error("OpenAI-compatible chat completions returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("parse OpenAI-compatible chat completions stream: {0}")]
    StreamJson(#[from] serde_json::Error),
    #[error("OpenAI-compatible chat completions stream error: {0}")]
    StreamError(String),
    #[error("read OpenAI-compatible chat completions stream: {0}")]
    StreamRead(#[from] std::io::Error),
    #[error("model returned no choices")]
    NoChoices,
    #[error("OpenAI-compatible model config is required for model compaction")]
    MissingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ModelToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(content: Option<String>, tool_calls: Vec<ModelToolCall>) -> Self {
        Self {
            role: "assistant".to_string(),
            content,
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ModelToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelToolFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub struct ModelTurn {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ModelToolCall>,
    pub streamed_content: bool,
    pub streamed_reasoning: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelDelta {
    Content(String),
    Reasoning(String),
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ChatResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ModelToolCall>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionStreamChunk {
    #[serde(default)]
    choices: Vec<ChatStreamChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatStreamChoice {
    #[serde(default)]
    delta: ChatStreamDelta,
}

#[derive(Debug, Default, Deserialize)]
struct ChatStreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChatStreamToolCall>,
}

#[derive(Debug, Deserialize)]
struct ChatStreamToolCall {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    call_type: Option<String>,
    #[serde(default)]
    function: Option<ChatStreamToolFunction>,
}

#[derive(Debug, Deserialize)]
struct ChatStreamToolFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    call_type: String,
    name: String,
    arguments: String,
}

impl ModelStream {
    pub fn new(config: Option<ModelConfig>) -> Self {
        let client = if let Some(config) = &config {
            let seconds = config.request_timeout_seconds.max(1);
            reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(seconds))
                .build()
                .unwrap_or_else(|_| reqwest::blocking::Client::new())
        } else {
            reqwest::blocking::Client::new()
        };
        Self {
            config,
            client,
            tools: Value::Array(Vec::new()),
        }
    }

    pub fn with_tools(mut self, tools: Value) -> Self {
        self.tools = tools;
        self
    }

    #[cfg(test)]
    pub fn sample(&self, messages: &[ChatMessage]) -> Result<ModelTurn, ModelError> {
        self.sample_with_deltas(messages, |_| {})
    }

    pub fn sample_with_deltas(
        &self,
        messages: &[ChatMessage],
        mut on_delta: impl FnMut(ModelDelta),
    ) -> Result<ModelTurn, ModelError> {
        match &self.config {
            Some(config) => self.sample_openai_compatible(config, messages, &mut on_delta),
            None => Ok(self.sample_local(messages)),
        }
    }

    pub fn summarize(&self, messages: &[ChatMessage]) -> Result<String, ModelError> {
        match &self.config {
            Some(config) => self.summarize_openai_compatible(config, messages),
            None => Err(ModelError::MissingConfig),
        }
    }

    fn sample_openai_compatible(
        &self,
        config: &ModelConfig,
        messages: &[ChatMessage],
        on_delta: &mut impl FnMut(ModelDelta),
    ) -> Result<ModelTurn, ModelError> {
        let provider = config.provider.trim().to_lowercase();
        if !provider.is_empty() && provider != "openai" && provider != "openai_compatible" {
            return Err(ModelError::UnsupportedProvider(config.provider.clone()));
        }

        let mut payload = json!({
            "model": config.model,
            "messages": messages,
            "tools": self.tools,
            "tool_choice": "auto",
            "stream": true
        });
        if !config.reasoning_effort.is_empty() {
            payload["reasoning_effort"] = Value::String(config.reasoning_effort.clone());
        }
        let response = self
            .client
            .post(format!("{}/chat/completions", config.base_url_effective()))
            .bearer_auth(&config.api_key)
            .json(&payload)
            .send()?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(ModelError::Status {
                status: status.as_u16(),
                body,
            });
        }

        if response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"))
        {
            return self.read_openai_compatible_stream(response, on_delta);
        }

        let choice = response
            .json::<ChatCompletionResponse>()?
            .choices
            .into_iter()
            .next()
            .ok_or(ModelError::NoChoices)?;
        Ok(ModelTurn {
            content: choice.message.content.unwrap_or_default(),
            reasoning: choice.message.reasoning_content.unwrap_or_default(),
            tool_calls: choice.message.tool_calls,
            streamed_content: false,
            streamed_reasoning: false,
        })
    }

    fn summarize_openai_compatible(
        &self,
        config: &ModelConfig,
        messages: &[ChatMessage],
    ) -> Result<String, ModelError> {
        let provider = config.provider.trim().to_lowercase();
        if !provider.is_empty() && provider != "openai" && provider != "openai_compatible" {
            return Err(ModelError::UnsupportedProvider(config.provider.clone()));
        }
        let payload = json!({
            "model": config.model,
            "messages": messages,
            "stream": false
        });
        let response = self
            .client
            .post(format!("{}/chat/completions", config.base_url_effective()))
            .bearer_auth(&config.api_key)
            .json(&payload)
            .send()?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(ModelError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let choice = response
            .json::<ChatCompletionResponse>()?
            .choices
            .into_iter()
            .next()
            .ok_or(ModelError::NoChoices)?;
        Ok(choice.message.content.unwrap_or_default())
    }

    fn read_openai_compatible_stream(
        &self,
        response: reqwest::blocking::Response,
        on_delta: &mut impl FnMut(ModelDelta),
    ) -> Result<ModelTurn, ModelError> {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tool_calls: BTreeMap<usize, PartialToolCall> = BTreeMap::new();
        let mut saw_choice = false;
        let mut streamed_content = false;
        let mut streamed_reasoning = false;

        let reader = BufReader::new(response);
        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                break;
            }
            let raw: Value = serde_json::from_str(data)?;
            if let Some(message) = raw
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
            {
                return Err(ModelError::StreamError(message.to_string()));
            }
            let chunk: ChatCompletionStreamChunk = serde_json::from_value(raw)?;
            for choice in chunk.choices {
                saw_choice = true;
                if let Some(delta) = choice.delta.content.filter(|value| !value.is_empty()) {
                    streamed_content = true;
                    content.push_str(&delta);
                    on_delta(ModelDelta::Content(delta));
                }
                let reasoning_delta = choice
                    .delta
                    .reasoning_content
                    .or(choice.delta.reasoning)
                    .filter(|value| !value.is_empty());
                if let Some(delta) = reasoning_delta {
                    streamed_reasoning = true;
                    reasoning.push_str(&delta);
                    on_delta(ModelDelta::Reasoning(delta));
                }
                for part in choice.delta.tool_calls {
                    let index = part.index.unwrap_or(tool_calls.len());
                    let partial = tool_calls.entry(index).or_default();
                    if let Some(id) = part.id.filter(|value| !value.is_empty()) {
                        partial.id = id;
                    }
                    if let Some(call_type) = part.call_type.filter(|value| !value.is_empty()) {
                        partial.call_type = call_type;
                    }
                    if let Some(function) = part.function {
                        if let Some(name) = function.name.filter(|value| !value.is_empty()) {
                            partial.name.push_str(&name);
                        }
                        if let Some(arguments) =
                            function.arguments.filter(|value| !value.is_empty())
                        {
                            partial.arguments.push_str(&arguments);
                        }
                    }
                }
            }
        }

        if !saw_choice {
            return Err(ModelError::NoChoices);
        }

        let tool_calls = tool_calls
            .into_iter()
            .map(|(index, partial)| ModelToolCall {
                id: if partial.id.is_empty() {
                    format!("call_stream_{}", index)
                } else {
                    partial.id
                },
                call_type: if partial.call_type.is_empty() {
                    "function".to_string()
                } else {
                    partial.call_type
                },
                function: ModelToolFunction {
                    name: partial.name,
                    arguments: partial.arguments,
                },
            })
            .filter(|call| !call.function.name.is_empty())
            .collect();

        Ok(ModelTurn {
            content,
            reasoning,
            tool_calls,
            streamed_content,
            streamed_reasoning,
        })
    }

    fn sample_local(&self, messages: &[ChatMessage]) -> ModelTurn {
        let user_message = messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .and_then(|m| m.content.as_deref())
            .unwrap_or_default();
        let must_continue = messages.iter().any(|m| {
            m.role == "system"
                && m.content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("ACTIVE_PLAN_BLOCKS_COMPLETION")
        });
        let plan_first_required = messages.iter().any(|m| {
            m.role == "system"
                && m.content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("PLAN_FIRST_REQUIRED")
        });
        let plan_tool_results: Vec<&str> = messages
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_deref())
            .filter(|content| {
                content.contains("\"tool\":\"update_plan\"")
                    || content.contains("\"tool\":\"todowrite\"")
            })
            .collect();
        let should_simulate_echo = messages.iter().any(|m| {
            m.role == "system"
                && m.content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("SIMULATE_RUNTIME_ECHO")
        });
        let echo_already_ran = messages
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_deref())
            .any(|content| content.starts_with("runtime_echo:"));
        let should_simulate_skill = messages.iter().any(|m| {
            m.role == "system"
                && m.content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("SIMULATE_SKILL_TOOL")
        });
        let skill_already_ran = messages
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_deref())
            .any(|content| content.contains("\"tool\":\"skill\""));
        let should_simulate_knowledge = messages.iter().any(|m| {
            m.role == "system"
                && m.content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("SIMULATE_KNOWLEDGE_SEARCH")
        });
        let knowledge_already_ran = messages
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_deref())
            .any(|content| content.contains("\"tool\":\"knowledge_search\""));
        let should_simulate_mcp = messages.iter().any(|m| {
            m.role == "system"
                && m.content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("SIMULATE_MCP_CALL")
        });
        let mcp_search_already_ran = messages
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_deref())
            .any(|content| content.contains("\"tool\":\"tool_search\""));
        let mcp_already_ran = messages
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_deref())
            .any(|content| {
                content.contains("\"tool_kind\":\"mcp\"")
                    || content.contains("\"tool\":\"mcp_call\"")
            });
        let mcp_budget_blocked = messages.iter().any(|m| {
            m.role == "system"
                && m.content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("MCP_SCHEMA_BUDGET_BLOCKED")
        });

        if plan_tool_results.is_empty()
            && (plan_first_required || should_local_plan_first(messages))
        {
            return ModelTurn {
                content: String::new(),
                reasoning: "analyzing user request into todo list".to_string(),
                tool_calls: vec![local_tool_call(
                    "update_plan",
                    json!({
                        "items": local_plan_items_for_user_message(user_message)
                    }),
                )],
                streamed_content: false,
                streamed_reasoning: false,
            };
        }

        if should_simulate_echo && !echo_already_ran {
            return ModelTurn {
                content: String::new(),
                reasoning: "calling diagnostic runtime_echo tool".to_string(),
                tool_calls: vec![local_tool_call(
                    "runtime_echo",
                    json!({ "message": user_message }),
                )],
                streamed_content: false,
                streamed_reasoning: false,
            };
        }

        if should_simulate_skill && !skill_already_ran {
            return ModelTurn {
                content: String::new(),
                reasoning: "loading requested skill".to_string(),
                tool_calls: vec![local_tool_call("skill", json!({ "name": "demo" }))],
                streamed_content: false,
                streamed_reasoning: false,
            };
        }

        if should_simulate_knowledge && !knowledge_already_ran {
            return ModelTurn {
                content: String::new(),
                reasoning: "searching injected knowledge snippets".to_string(),
                tool_calls: vec![local_tool_call(
                    "knowledge_search",
                    json!({ "query": user_message, "top_k": 3 }),
                )],
                streamed_content: false,
                streamed_reasoning: false,
            };
        }

        if should_simulate_mcp && !mcp_already_ran {
            if !mcp_search_already_ran {
                return ModelTurn {
                    content: String::new(),
                    reasoning: "searching deferred MCP catalog before loading schema".to_string(),
                    tool_calls: vec![local_tool_call(
                        "tool_search",
                        json!({ "query": "select:lookup" }),
                    )],
                    streamed_content: false,
                    streamed_reasoning: false,
                };
            }
            let direct_mcp_tool = self.tools.as_array().and_then(|items| {
                items.iter().find_map(|item| {
                    item.get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                        .filter(|name| !is_builtin_local_tool_name(name))
                })
            });
            if let Some(tool_name) = direct_mcp_tool {
                return ModelTurn {
                    content: String::new(),
                    reasoning: "calling available first-class MCP tool through runtime registry"
                        .to_string(),
                    tool_calls: vec![local_tool_call(tool_name, json!({ "query": user_message }))],
                    streamed_content: false,
                    streamed_reasoning: false,
                };
            }
            if mcp_budget_blocked {
                return ModelTurn {
                    content: "MCP selected tool schema is budget_blocked; compress history or select a smaller tool before calling it.".to_string(),
                    reasoning: "selected MCP schema is budget blocked".to_string(),
                    tool_calls: Vec::new(),
                    streamed_content: false,
                    streamed_reasoning: false,
                };
            }
            return ModelTurn {
                content: String::new(),
                reasoning: "calling available MCP tool through compatibility bridge".to_string(),
                tool_calls: vec![local_tool_call(
                    "mcp_call",
                    json!({ "tool": "demo::lookup", "arguments": { "query": user_message } }),
                )],
                streamed_content: false,
                streamed_reasoning: false,
            };
        }

        if must_continue
            && !plan_tool_results.last().is_some_and(|content| {
                !content.contains("\"in_progress\"") && !content.contains("\"pending\"")
            })
        {
            return ModelTurn {
                content: String::new(),
                reasoning: "active plan blocks completion; updating plan before final answer"
                    .to_string(),
                tool_calls: vec![local_tool_call(
                    "update_plan",
                    json!({
                        "items": local_completed_plan_items_for_user_message(user_message)
                    }),
                )],
                streamed_content: false,
                streamed_reasoning: false,
            };
        }

        if plan_tool_results.last().is_some_and(|content| {
            content.contains("\"in_progress\"") || content.contains("\"pending\"")
        }) {
            return ModelTurn {
                content: "建议你下一步可以让我继续执行剩余计划。".to_string(),
                reasoning: "simulating premature delegation while plan is still active".to_string(),
                tool_calls: Vec::new(),
                streamed_content: false,
                streamed_reasoning: false,
            };
        }

        ModelTurn {
            content: format!(
                "Agent Runtime kernel completed the turn after model/tool follow-up. Current independent Rust runtime boundary is active for: {}",
                user_message.trim()
            ),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            streamed_content: false,
            streamed_reasoning: false,
        }
    }
}

fn normalize_reasoning_effort(value: &str) -> String {
    match value.trim().to_lowercase().as_str() {
        "low" | "medium" | "high" | "max" | "xhigh" => value.trim().to_lowercase(),
        _ => String::new(),
    }
}

fn should_local_plan_first(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "system"
            && m.content
                .as_deref()
                .unwrap_or_default()
                .contains("Every user request, even a simple one, must first be analyzed")
    })
}

fn local_plan_items_for_user_message(message: &str) -> Value {
    let summary = summarize_user_request_for_local_plan(message);
    json!([
        {"id": "analyze", "step": format!("Analyze user request: {}", summary), "status": "completed"},
        {"id": "work", "step": format!("Handle user request: {}", summary), "status": "in_progress"},
        {"id": "finalize", "step": "汇总结果并输出最终回复", "status": "pending"}
    ])
}

fn local_completed_plan_items_for_user_message(message: &str) -> Value {
    let summary = summarize_user_request_for_local_plan(message);
    json!([
        {"id": "analyze", "step": format!("Analyze user request: {}", summary), "status": "completed"},
        {"id": "work", "step": format!("Handle user request: {}", summary), "status": "completed"},
        {"id": "finalize", "step": "汇总结果并输出最终回复", "status": "completed"}
    ])
}

fn summarize_user_request_for_local_plan(message: &str) -> String {
    let collapsed = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        return "处理当前输入".to_string();
    }
    let mut summary = trimmed.chars().take(48).collect::<String>();
    if trimmed.chars().count() > 48 {
        summary.push('…');
    }
    summary
}

fn local_tool_call(name: &str, args: Value) -> ModelToolCall {
    ModelToolCall {
        id: format!("call_local_{}", name),
        call_type: "function".to_string(),
        function: ModelToolFunction {
            name: name.to_string(),
            arguments: args.to_string(),
        },
    }
}

fn is_builtin_local_tool_name(name: &str) -> bool {
    matches!(
        name,
        "update_plan"
            | "todowrite"
            | "runtime_echo"
            | "skill"
            | "mcp_call"
            | "tool_search"
            | "knowledge_search"
            | "ls"
            | "read_file"
            | "write_file"
            | "edit_file"
            | "glob"
            | "grep"
            | "execute"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn streams_content_and_reasoning_deltas() {
        let endpoint = start_mock_chat_server(
            "text/event-stream",
            concat!(
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think \"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n",
                "data: [DONE]\n\n"
            ),
            true,
            Some("\"reasoning_effort\":\"xhigh\""),
        );
        let model = ModelStream::new(Some(ModelConfig {
            provider: "openai_compatible".to_string(),
            api_key: "test-key".to_string(),
            base_url: endpoint,
            model: "test-model".to_string(),
            reasoning_effort: "xhigh".to_string(),
            request_timeout_seconds: 120,
        }));
        let mut deltas = Vec::new();

        let turn = model
            .sample_with_deltas(&[ChatMessage::user("hello")], |delta| deltas.push(delta))
            .unwrap();

        assert_eq!(turn.content, "hello world");
        assert_eq!(turn.reasoning, "think ");
        assert!(turn.streamed_content);
        assert!(turn.streamed_reasoning);
        assert_eq!(
            deltas,
            vec![
                ModelDelta::Reasoning("think ".to_string()),
                ModelDelta::Content("hello".to_string()),
                ModelDelta::Content(" world".to_string())
            ]
        );
    }

    #[test]
    fn reconstructs_streamed_tool_calls() {
        let endpoint = start_mock_chat_server(
            "text/event-stream",
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"runtime_\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"echo\",\"arguments\":\"{\\\"message\\\":\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"hi\\\"}\"}}]}}]}\n\n",
                "data: [DONE]\n\n"
            ),
            true,
            None,
        );
        let model = ModelStream::new(Some(ModelConfig {
            provider: "openai".to_string(),
            api_key: "test-key".to_string(),
            base_url: endpoint,
            model: "test-model".to_string(),
            reasoning_effort: String::new(),
            request_timeout_seconds: 120,
        }));

        let turn = model.sample(&[ChatMessage::user("call tool")]).unwrap();

        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call_1");
        assert_eq!(turn.tool_calls[0].call_type, "function");
        assert_eq!(turn.tool_calls[0].function.name, "runtime_echo");
        assert_eq!(
            turn.tool_calls[0].function.arguments,
            "{\"message\":\"hi\"}"
        );
    }

    #[test]
    fn reads_request_timeout_from_context() {
        let mut context = Map::new();
        context.insert(
            "openai_api_key".to_string(),
            Value::String("key".to_string()),
        );
        context.insert(
            "openai_model".to_string(),
            Value::String("model".to_string()),
        );
        context.insert("tool_timeout_seconds".to_string(), Value::from(7_u64));

        let config = ModelConfig::from_context(&context).unwrap();

        assert_eq!(config.request_timeout_seconds, 7);
    }

    #[test]
    fn reads_reasoning_effort_from_context() {
        let mut context = Map::new();
        context.insert(
            "openai_api_key".to_string(),
            Value::String("key".to_string()),
        );
        context.insert(
            "openai_model".to_string(),
            Value::String("model".to_string()),
        );
        context.insert(
            "openai_reasoning_effort".to_string(),
            Value::String("XHIGH".to_string()),
        );

        let config = ModelConfig::from_context(&context).unwrap();

        assert_eq!(config.reasoning_effort, "xhigh");
    }

    #[test]
    fn summarizes_with_openai_compatible_chat_completion() {
        let endpoint = start_mock_chat_server(
            "application/json",
            r#"{"choices":[{"message":{"content":"model summary"}}]}"#,
            false,
            None,
        );
        let model = ModelStream::new(Some(ModelConfig {
            provider: "openai_compatible".to_string(),
            api_key: "test-key".to_string(),
            base_url: endpoint,
            model: "test-model".to_string(),
            reasoning_effort: String::new(),
            request_timeout_seconds: 120,
        }));

        let summary = model
            .summarize(&[ChatMessage::user("summarize me")])
            .unwrap();

        assert_eq!(summary, "model summary");
    }

    fn start_mock_chat_server(
        content_type: &'static str,
        response_body: &'static str,
        stream: bool,
        expected_body_fragment: Option<&'static str>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 8192];
            let bytes = socket.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]);
            assert!(request.contains("POST /chat/completions HTTP/1.1"));
            assert!(request.contains(&format!("\"stream\":{stream}")));
            if let Some(fragment) = expected_body_fragment {
                assert!(
                    request.contains(fragment),
                    "request body missing expected fragment {fragment}: {request}"
                );
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                content_type,
                response_body.len(),
                response_body
            );
            socket.write_all(response.as_bytes()).unwrap();
        });
        format!("http://{}", addr)
    }
}
