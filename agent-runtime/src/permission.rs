use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::time::Duration;

use crate::tool_registry::ToolInvocation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny,
    RequireApproval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionReply {
    Once,
    Always,
    Reject,
}

#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub id: String,
    pub conversation_id: String,
    pub session_id: String,
    pub message_id: String,
    pub tool_name: String,
    pub tool_call_id: String,
    pub permission: String,
    pub patterns: Vec<String>,
    pub always: bool,
    pub metadata: Value,
    pub payload: Value,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct PermissionReplyPayload {
    pub reply: PermissionReply,
    pub comment: String,
    pub edited_arguments: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct PermissionPolicy {
    approval_enabled: bool,
    allowlist: Vec<String>,
    denylist: Vec<String>,
    ask_url: Option<String>,
    timeout_seconds: u64,
    assistant_message_id: String,
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self {
            approval_enabled: false,
            allowlist: default_allowlist(),
            denylist: Vec::new(),
            ask_url: None,
            timeout_seconds: 600,
            assistant_message_id: String::new(),
        }
    }
}

impl PermissionPolicy {
    pub fn from_context(context: &Map<String, Value>) -> Self {
        let approval_enabled = context
            .get("approval_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let allowlist = string_list(context, "approval_allowlist")
            .or_else(|| string_list(context, "tool_approval_allowlist"))
            .unwrap_or_else(default_allowlist);
        let denylist = string_list(context, "approval_denylist")
            .or_else(|| string_list(context, "tool_approval_denylist"))
            .unwrap_or_default();
        let ask_url = context
            .get("hitl_permission_ask_url")
            .or_else(|| context.get("permission_ask_url"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        let timeout_seconds = context
            .get("hitl_timeout_seconds")
            .or_else(|| context.get("approval_timeout_seconds"))
            .and_then(Value::as_u64)
            .unwrap_or(600);
        let assistant_message_id = context
            .get("assistant_message_id")
            .or_else(|| context.get("assistantMessageId"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        Self {
            approval_enabled,
            allowlist,
            denylist,
            ask_url,
            timeout_seconds,
            assistant_message_id,
        }
    }

    pub fn evaluate_invocation(&self, invocation: &ToolInvocation) -> PermissionDecision {
        if matches_any_alias(&invocation.permission_aliases, &self.denylist)
            || matches_any(&invocation.permission_name, &self.denylist)
        {
            return PermissionDecision::Deny;
        }
        if matches_any_alias(&invocation.permission_aliases, &self.allowlist)
            || matches_any(&invocation.permission_name, &self.allowlist)
        {
            return PermissionDecision::Allow;
        }
        if self.approval_enabled
            && (invocation.requires_approval
                || looks_dangerous(&invocation.permission_name)
                || invocation
                    .permission_aliases
                    .iter()
                    .any(|alias| looks_dangerous(alias)))
        {
            PermissionDecision::RequireApproval
        } else {
            PermissionDecision::Allow
        }
    }

    pub fn action_for_invocation(&self, invocation: &ToolInvocation) -> PermissionAction {
        match self.evaluate_invocation(invocation) {
            PermissionDecision::Allow => PermissionAction::Allow,
            PermissionDecision::Deny => PermissionAction::Deny,
            PermissionDecision::RequireApproval => PermissionAction::Ask,
        }
    }

    pub fn has_rust_ask_endpoint(&self) -> bool {
        self.ask_url.is_some()
    }

    pub fn build_request(
        &self,
        conversation_id: &str,
        runtime_session_id: &str,
        request_id: &str,
        invocation: &ToolInvocation,
        tool_call_id: &str,
        arguments: Value,
    ) -> PermissionRequest {
        let patterns = permission_patterns(invocation, &arguments);
        PermissionRequest {
            id: request_id.to_string(),
            conversation_id: conversation_id.to_string(),
            session_id: runtime_session_id.to_string(),
            message_id: self.assistant_message_id.clone(),
            tool_name: invocation.display_name.clone(),
            tool_call_id: tool_call_id.to_string(),
            permission: invocation.permission_name.clone(),
            patterns,
            always: false,
            metadata: json!({
                "source": "agent-runtime",
                "permissionAliases": invocation.permission_aliases.clone(),
                "requiresApproval": invocation.requires_approval,
            }),
            payload: json!({
                "arguments": arguments,
            }),
            timeout_seconds: self.timeout_seconds,
        }
    }

    pub fn ask(&self, request: &PermissionRequest) -> Result<PermissionReplyPayload, String> {
        let url = self
            .ask_url
            .as_deref()
            .ok_or_else(|| "permission ask URL is not configured".to_string())?;
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(
                request.timeout_seconds.saturating_add(30).max(30),
            ))
            .build()
            .map_err(|err| format!("create permission client: {err}"))?;
        let resp = client
            .post(url)
            .json(&json!({
                "id": request.id,
                "conversationId": request.conversation_id,
                "sessionId": request.session_id,
                "messageId": request.message_id,
                "toolName": request.tool_name,
                "toolCallId": request.tool_call_id,
                "permission": request.permission,
                "patterns": request.patterns,
                "always": request.always,
                "metadata": request.metadata,
                "payload": request.payload,
                "timeoutSeconds": request.timeout_seconds,
            }))
            .send()
            .map_err(|err| format!("send permission ask: {err}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .map_err(|err| format!("read permission reply: {err}"))?;
        let body: Value = serde_json::from_str(&text).map_err(|err| {
            format!(
                "decode permission reply: {err}; status={status}; body={}",
                text.trim()
            )
        })?;
        if !status.is_success() {
            return Err(format!("permission ask returned {status}: {body}"));
        }
        let reply = body
            .get("reply")
            .and_then(Value::as_str)
            .and_then(parse_permission_reply)
            .unwrap_or_else(|| {
                if body.get("action").and_then(Value::as_str) == Some("deny") {
                    PermissionReply::Reject
                } else {
                    PermissionReply::Once
                }
            });
        let comment = body
            .get("comment")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let edited_arguments = body.get("editedArguments").cloned();
        Ok(PermissionReplyPayload {
            reply,
            comment,
            edited_arguments,
        })
    }
}

fn parse_permission_reply(value: &str) -> Option<PermissionReply> {
    match value.trim().to_ascii_lowercase().as_str() {
        "once" | "approve" | "allow" => Some(PermissionReply::Once),
        "always" => Some(PermissionReply::Always),
        "reject" | "deny" => Some(PermissionReply::Reject),
        _ => None,
    }
}

fn permission_patterns(invocation: &ToolInvocation, arguments: &Value) -> Vec<String> {
    let mut out = Vec::new();
    push_unique(&mut out, &invocation.permission_name);
    push_unique(&mut out, &invocation.display_name);
    for alias in &invocation.permission_aliases {
        push_unique(&mut out, alias);
    }
    for key in ["command", "cmd", "path", "file", "target"] {
        if let Some(value) = arguments.get(key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                push_unique(&mut out, &format!("{key}:{}", value.trim()));
            }
        }
    }
    out
}

fn push_unique(out: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() && !out.iter().any(|item| item == trimmed) {
        out.push(trimmed.to_string());
    }
}

fn default_allowlist() -> Vec<String> {
    vec![
        "update_plan".to_string(),
        "todowrite".to_string(),
        "runtime_echo".to_string(),
        "skill".to_string(),
        "knowledge_search".to_string(),
    ]
}

fn string_list(context: &Map<String, Value>, key: &str) -> Option<Vec<String>> {
    let value = context.get(key)?;
    if let Some(items) = value.as_array() {
        let out: Vec<String> = items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        return Some(out);
    }
    value.as_str().map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    })
}

fn matches_any(tool: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| wildcard_match(pattern, tool))
}

fn matches_any_alias(aliases: &[String], patterns: &[String]) -> bool {
    aliases.iter().any(|alias| {
        patterns
            .iter()
            .any(|pattern| wildcard_match(pattern, alias))
    })
}

fn looks_dangerous(tool: &str) -> bool {
    let lower = tool.to_lowercase();
    lower.contains("write")
        || lower.contains("edit")
        || lower.contains("delete")
        || lower.contains("remove")
        || lower.contains("shell")
        || lower.contains("exec")
        || lower.contains("command")
        || lower.contains("apply_patch")
        || lower.contains("mcp_call")
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.trim();
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return value.ends_with(suffix);
    }
    pattern.eq_ignore_ascii_case(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};

    #[test]
    fn dangerous_tool_requires_approval_when_enabled() {
        let mut context = Map::new();
        context.insert("approval_enabled".to_string(), Value::Bool(true));
        let policy = PermissionPolicy::from_context(&context);
        assert_eq!(
            policy.evaluate_invocation(&invocation("mcp_call", false)),
            PermissionDecision::RequireApproval
        );
        assert_eq!(
            policy.evaluate_invocation(&invocation("knowledge_search", false)),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn allowlist_overrides_dangerous_name() {
        let mut context = Map::new();
        context.insert("approval_enabled".to_string(), Value::Bool(true));
        context.insert("approval_allowlist".to_string(), json!(["mcp_call"]));
        let policy = PermissionPolicy::from_context(&context);
        assert_eq!(
            policy.evaluate_invocation(&invocation("mcp_call", false)),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn denylist_rejects_tool() {
        let mut context = Map::new();
        context.insert("approval_denylist".to_string(), json!(["skill"]));
        let policy = PermissionPolicy::from_context(&context);
        assert_eq!(
            policy.evaluate_invocation(&invocation("skill", false)),
            PermissionDecision::Deny
        );
    }

    #[test]
    fn invocation_uses_mcp_identity_and_requires_approval_flag() {
        let mut context = Map::new();
        context.insert("approval_enabled".to_string(), Value::Bool(true));
        let policy = PermissionPolicy::from_context(&context);
        let invocation = ToolInvocation {
            display_name: "demo::lookup".to_string(),
            permission_name: "demo::lookup".to_string(),
            permission_aliases: vec![
                "mcp__demo__lookup".to_string(),
                "demo::lookup".to_string(),
                "lookup".to_string(),
            ],
            requires_approval: true,
        };

        assert_eq!(
            policy.evaluate_invocation(&invocation),
            PermissionDecision::RequireApproval
        );

        context.insert("approval_allowlist".to_string(), json!(["demo::lookup"]));
        let policy = PermissionPolicy::from_context(&context);
        assert_eq!(
            policy.evaluate_invocation(&invocation),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn builds_permission_request_with_patterns_and_message_id() {
        let mut context = Map::new();
        context.insert(
            "hitl_permission_ask_url".to_string(),
            Value::String("http://127.0.0.1:51283/api/internal/hitl/permission-ask".to_string()),
        );
        context.insert(
            "assistant_message_id".to_string(),
            Value::String("assistant-1".to_string()),
        );
        let policy = PermissionPolicy::from_context(&context);
        let request = policy.build_request(
            "conv-1",
            "session-1",
            "approval-call-1",
            &ToolInvocation {
                display_name: "execute".to_string(),
                permission_name: "execute".to_string(),
                permission_aliases: vec!["execute".to_string()],
                requires_approval: true,
            },
            "call-1",
            json!({"command": "npm test"}),
        );
        assert_eq!(request.message_id, "assistant-1");
        assert!(request.patterns.contains(&"execute".to_string()));
        assert!(request.patterns.contains(&"command:npm test".to_string()));
    }

    fn invocation(name: &str, requires_approval: bool) -> ToolInvocation {
        ToolInvocation {
            display_name: name.to_string(),
            permission_name: name.to_string(),
            permission_aliases: vec![name.to_string()],
            requires_approval,
        }
    }
}
