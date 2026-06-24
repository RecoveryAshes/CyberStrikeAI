use serde_json::{Map, Value};

use crate::tool_registry::ToolInvocation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny,
    RequireApproval,
}

#[derive(Debug, Clone)]
pub struct PermissionPolicy {
    approval_enabled: bool,
    allowlist: Vec<String>,
    denylist: Vec<String>,
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self {
            approval_enabled: false,
            allowlist: default_allowlist(),
            denylist: Vec::new(),
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
        Self {
            approval_enabled,
            allowlist,
            denylist,
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

    fn invocation(name: &str, requires_approval: bool) -> ToolInvocation {
        ToolInvocation {
            display_name: name.to_string(),
            permission_name: name.to_string(),
            permission_aliases: vec![name.to_string()],
            requires_approval,
        }
    }
}
