use serde_json::{json, Map, Value};
use thiserror::Error;

use crate::filesystem_runtime::FilesystemRuntime;
use crate::knowledge_runtime::KnowledgeRuntime;
use crate::mcp_bridge::{wrapped_mcp_result_is_error, McpBridge, MODEL_TOOL_NAME_MAX_LEN};
use crate::mcp_registry::{McpLoadedTools, McpRegistryTool, McpToolRegistry};
use crate::model_stream::ModelToolCall;
use crate::plan_store::PlanStore;
use crate::skill_runtime::SkillRuntime;
use crate::tool_runtime::{ToolError, ToolOutcome, ToolRuntime};

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolSpec {
    pub fn openai_schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters
            }
        })
    }
}

#[derive(Debug)]
pub struct ToolExecutionContext<'a> {
    pub plan: &'a mut PlanStore,
    pub skills: &'a SkillRuntime,
    pub mcp: &'a McpBridge,
    pub mcp_registry: &'a McpToolRegistry,
    pub mcp_loaded: &'a mut McpLoadedTools,
    pub knowledge: &'a KnowledgeRuntime,
    pub filesystem: &'a FilesystemRuntime,
    pub tool_timeout_seconds: u64,
}

#[derive(Debug, Default)]
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
    mcp_tools: Vec<RegisteredMcpTool>,
}

#[derive(Debug, Clone)]
struct RegisteredTool {
    spec: ToolSpec,
    kind: ToolKind,
}

#[derive(Debug, Clone)]
enum ToolKind {
    Plan,
    RuntimeEcho,
    Skill,
    McpCall,
    McpToolSearch,
    KnowledgeSearch,
    Filesystem(RegisteredFilesystemTool),
    McpDirect(RegisteredMcpTool),
}

#[derive(Debug, Clone)]
pub struct RegisteredMcpTool {
    pub model_name: String,
    pub identity: String,
    pub source: String,
    pub name: String,
    pub call_name: String,
    pub requires_approval: bool,
}

#[derive(Debug, Clone)]
struct RegisteredFilesystemTool {
    name: String,
    requires_approval: bool,
}

#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub display_name: String,
    pub permission_name: String,
    pub permission_aliases: Vec<String>,
    pub requires_approval: bool,
}

#[derive(Debug, Error)]
pub enum ToolRegistryError {
    #[error("unsupported runtime tool: {0}")]
    UnsupportedTool(String),
    #[error("{0}")]
    MisroutedRuntimeTool(String),
    #[error(transparent)]
    Runtime(#[from] crate::tool_runtime::ToolError),
}

impl ToolRegistry {
    pub fn builtin() -> Self {
        let mut registry = Self::default();
        registry.add_builtin(update_plan_spec(), ToolKind::Plan);
        registry.add_builtin(todowrite_spec(), ToolKind::Plan);
        registry.add_builtin(runtime_echo_spec(), ToolKind::RuntimeEcho);
        registry.add_builtin(skill_spec(), ToolKind::Skill);
        registry.add_builtin(mcp_call_spec(), ToolKind::McpCall);
        registry.add_builtin(tool_search_spec(), ToolKind::McpToolSearch);
        registry.add_builtin(knowledge_search_spec(), ToolKind::KnowledgeSearch);
        registry
    }

    pub fn from_capabilities(
        filesystem: &FilesystemRuntime,
        loaded_mcp_tools: &[McpRegistryTool],
    ) -> Self {
        let mut registry = Self::builtin();
        for spec in filesystem.tool_specs() {
            registry.add_builtin(
                ToolSpec {
                    name: spec.name.to_string(),
                    description: spec.description.to_string(),
                    parameters: spec.parameters,
                },
                ToolKind::Filesystem(RegisteredFilesystemTool {
                    name: spec.name.to_string(),
                    requires_approval: spec.requires_approval,
                }),
            );
        }

        registry.mcp_tools = loaded_mcp_tools
            .iter()
            .map(|tool| RegisteredMcpTool {
                model_name: tool.model_tool_name.clone(),
                identity: tool.identity.clone(),
                source: tool.source.clone(),
                name: tool.name.clone(),
                call_name: tool.call_name.clone(),
                requires_approval: tool.requires_approval,
            })
            .collect();

        let mut used_names: Vec<String> = registry
            .tools
            .iter()
            .map(|tool| tool.spec.name.clone())
            .collect();
        for tool in registry.mcp_tools.clone() {
            let model_name = unique_tool_name(&tool.model_name, &mut used_names);
            let registered = RegisteredMcpTool {
                model_name: model_name.clone(),
                ..tool
            };
            let Some(source) = loaded_mcp_tools
                .iter()
                .find(|source| source.identity == registered.identity)
            else {
                continue;
            };
            registry.tools.push(RegisteredTool {
                spec: ToolSpec {
                    name: model_name,
                    description: format!(
                        "CyberStrikeAI MCP tool {} (schema_hash {}). {}",
                        registered.identity, source.schema_hash, source.short_description
                    )
                    .trim()
                    .to_string(),
                    parameters: source.input_schema.clone(),
                },
                kind: ToolKind::McpDirect(registered),
            });
        }
        registry
    }

    pub fn schemas(&self) -> Value {
        Value::Array(
            self.tools
                .iter()
                .map(|tool| tool.spec.openai_schema())
                .collect(),
        )
    }

    pub fn invocation(&self, call: &ModelToolCall) -> ToolInvocation {
        let model_tool_name = call.function.name.trim().to_string();
        let Some(tool) = self.find_tool(&model_tool_name) else {
            return ToolInvocation {
                display_name: model_tool_name.clone(),
                permission_name: model_tool_name.clone(),
                permission_aliases: vec![model_tool_name],
                requires_approval: false,
            };
        };

        match &tool.kind {
            ToolKind::McpDirect(mcp_tool) => ToolInvocation {
                display_name: mcp_tool.identity.clone(),
                permission_name: mcp_tool.identity.clone(),
                permission_aliases: vec![
                    model_tool_name,
                    mcp_tool.identity.clone(),
                    mcp_tool.call_name.clone(),
                    mcp_tool.name.clone(),
                ],
                requires_approval: mcp_tool.requires_approval,
            },
            ToolKind::McpCall => {
                if let Some((target, arguments)) =
                    self.canonical_runtime_tool_from_mcp_call(&call.function.arguments)
                {
                    let canonical = ModelToolCall {
                        id: call.id.clone(),
                        call_type: call.call_type.clone(),
                        function: crate::model_stream::ModelToolFunction {
                            name: target.clone(),
                            arguments: arguments.to_string(),
                        },
                    };
                    let mut invocation = self.invocation(&canonical);
                    invocation.permission_aliases.push("mcp_call".to_string());
                    return invocation;
                }
                if let Some(mcp_tool) = self.resolve_mcp_call(&call.function.arguments) {
                    ToolInvocation {
                        display_name: mcp_tool.identity.clone(),
                        permission_name: mcp_tool.identity.clone(),
                        permission_aliases: vec![
                            mcp_tool.identity.clone(),
                            mcp_tool.call_name.clone(),
                            mcp_tool.name.clone(),
                            mcp_tool.model_name.clone(),
                        ],
                        requires_approval: mcp_tool.requires_approval,
                    }
                } else {
                    ToolInvocation {
                        display_name: model_tool_name.clone(),
                        permission_name: model_tool_name.clone(),
                        permission_aliases: vec![model_tool_name],
                        requires_approval: false,
                    }
                }
            }
            ToolKind::Skill => builtin_invocation(&model_tool_name),
            ToolKind::KnowledgeSearch => builtin_invocation(&model_tool_name),
            ToolKind::McpToolSearch => builtin_invocation(&model_tool_name),
            ToolKind::Filesystem(fs_tool) => ToolInvocation {
                display_name: model_tool_name.clone(),
                permission_name: model_tool_name.clone(),
                permission_aliases: vec![model_tool_name],
                requires_approval: fs_tool.requires_approval,
            },
            ToolKind::Plan => builtin_invocation(&model_tool_name),
            ToolKind::RuntimeEcho => builtin_invocation(&model_tool_name),
        }
    }

    pub fn execute(
        &self,
        call: &ModelToolCall,
        ctx: &mut ToolExecutionContext<'_>,
        on_delta: Option<&mut dyn FnMut(String)>,
    ) -> Result<ToolOutcome, ToolRegistryError> {
        let name = call.function.name.trim();
        let Some(tool) = self.find_tool(name) else {
            if ctx.mcp_registry.find(name).is_some() {
                return Ok(ToolOutcome::FailedText(deferred_mcp_tool_error(name)));
            }
            return Err(ToolRegistryError::UnsupportedTool(name.to_string()));
        };
        match &tool.kind {
            ToolKind::Skill => Ok(ToolOutcome::Text(
                ctx.skills
                    .execute_call(&call.function.arguments)
                    .map_err(ToolError::from)?,
            )),
            ToolKind::McpCall => {
                if let Some((target, arguments)) =
                    self.canonical_runtime_tool_from_mcp_call(&call.function.arguments)
                {
                    if self.find_tool(&target).is_none() {
                        return Err(ToolRegistryError::MisroutedRuntimeTool(
                            runtime_tool_direct_call_message(&target),
                        ));
                    }
                    let canonical = ModelToolCall {
                        id: call.id.clone(),
                        call_type: call.call_type.clone(),
                        function: crate::model_stream::ModelToolFunction {
                            name: target,
                            arguments: arguments.to_string(),
                        },
                    };
                    return self.execute(&canonical, ctx, on_delta);
                }
                if let Some(requested) =
                    requested_tool_name_from_arguments(&call.function.arguments)
                {
                    if ctx.mcp_registry.find(&requested).is_some() {
                        return Ok(ToolOutcome::FailedText(deferred_mcp_tool_error(&requested)));
                    }
                }
                let result = ctx
                    .mcp
                    .execute_call(&call.function.arguments)
                    .map_err(|err| ToolError::Mcp(err.to_string()))?;
                if wrapped_mcp_result_is_error(&result) {
                    Ok(ToolOutcome::FailedText(result))
                } else {
                    Ok(ToolOutcome::Text(result))
                }
            }
            ToolKind::McpToolSearch => {
                let result = ctx
                    .mcp_registry
                    .search_tool(&call.function.arguments, ctx.mcp_loaded);
                Ok(ToolOutcome::Text(result.content))
            }
            ToolKind::McpDirect(mcp_tool) => {
                if !ctx.mcp_loaded.contains(&mcp_tool.identity) {
                    return Ok(ToolOutcome::FailedText(deferred_mcp_tool_error(
                        &mcp_tool.identity,
                    )));
                }
                let args: Value =
                    serde_json::from_str(&call.function.arguments).map_err(|source| {
                        ToolError::InvalidArguments {
                            tool_name: name.to_string(),
                            source,
                        }
                    })?;
                let source_tool = ctx.mcp_registry.find(&mcp_tool.identity).cloned();
                let result = if source_tool
                    .as_ref()
                    .is_some_and(|tool| tool.is_local_builtin())
                {
                    let payload = ctx
                        .mcp_registry
                        .execute_local_tool(
                            &mcp_tool.identity,
                            args.clone(),
                            ctx.tool_timeout_seconds,
                        )
                        .map_err(|err| ToolError::Mcp(err.to_string()))?;
                    wrap_mcp_result(
                        &mcp_tool.model_name,
                        &mcp_tool.identity,
                        &mcp_tool.source,
                        &mcp_tool.name,
                        args,
                        payload,
                    )
                } else {
                    ctx.mcp
                        .execute_direct_tool(
                            &mcp_tool.source,
                            &mcp_tool.name,
                            &mcp_tool.call_name,
                            args,
                            &mcp_tool.model_name,
                        )
                        .map_err(|err| ToolError::Mcp(err.to_string()))?
                };
                if let Some(tool) = source_tool.as_ref() {
                    ctx.mcp_loaded.mark_used(tool);
                }
                if wrapped_mcp_result_is_error(&result) {
                    Ok(ToolOutcome::FailedText(result))
                } else {
                    Ok(ToolOutcome::Text(result))
                }
            }
            ToolKind::KnowledgeSearch => Ok(ToolOutcome::Text(
                ctx.knowledge
                    .execute_call(&call.function.arguments)
                    .map_err(|err| ToolError::Knowledge(err.to_string()))?,
            )),
            ToolKind::Filesystem(fs_tool) => {
                let result = ctx
                    .filesystem
                    .execute_call_with_delta(&fs_tool.name, &call.function.arguments, on_delta)
                    .map_err(|err| ToolError::Filesystem(err.to_string()))?;
                Ok(ToolOutcome::Text(result))
            }
            ToolKind::Plan | ToolKind::RuntimeEcho => {
                Ok(ToolRuntime::default().execute(call, ctx.plan)?)
            }
        }
    }

    fn add_builtin(&mut self, spec: ToolSpec, kind: ToolKind) {
        self.tools.push(RegisteredTool { spec, kind });
    }

    fn find_tool(&self, name: &str) -> Option<&RegisteredTool> {
        self.tools.iter().find(|tool| tool.spec.name == name)
    }

    fn resolve_mcp_call(&self, arguments: &str) -> Option<RegisteredMcpTool> {
        let args: Value = serde_json::from_str(arguments).ok()?;
        let requested = args
            .get("tool")
            .or_else(|| args.get("name"))
            .or_else(|| args.get("tool_name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())?;
        self.mcp_tools
            .iter()
            .find(|tool| {
                requested == tool.identity
                    || requested == tool.name
                    || requested == tool.model_name
                    || requested == tool.call_name
            })
            .cloned()
    }

    fn canonical_runtime_tool_from_mcp_call(&self, arguments: &str) -> Option<(String, Value)> {
        let args: Value = serde_json::from_str(arguments).ok()?;
        let requested = requested_tool_name_from_value(&args)?;
        runtime_tool_direct_call_hint(&requested)?;
        let call_args = args
            .get("arguments")
            .or_else(|| args.get("args"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        Some((requested, normalize_runtime_tool_arguments(&call_args)))
    }
}

fn wrap_mcp_result(
    model_tool_name: &str,
    identity: &str,
    source: &str,
    name: &str,
    arguments: Value,
    result: Value,
) -> String {
    let status = if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        "failed"
    } else {
        "completed"
    };
    json!({
        "tool": model_tool_name,
        "tool_kind": "mcp",
        "mcp_tool": identity,
        "server": source,
        "name": name,
        "arguments": arguments,
        "status": status,
        "result": result
    })
    .to_string()
}

fn requested_tool_name_from_value(args: &Value) -> Option<String> {
    args.get("tool")
        .or_else(|| args.get("name"))
        .or_else(|| args.get("tool_name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

fn requested_tool_name_from_arguments(arguments: &str) -> Option<String> {
    let args: Value = serde_json::from_str(arguments).ok()?;
    requested_tool_name_from_value(&args)
}

fn deferred_mcp_tool_error(requested: &str) -> String {
    json!({
        "tool_kind": "mcp",
        "status": "deferred_schema_required",
        "requested_tool": requested,
        "message": format!(
            "MCP tool '{}' is not loaded with a full JSON schema in this request. Call tool_search with select:{} first, then wait for the next model request before calling the tool. Do not guess parameters.",
            requested,
            requested
        )
    })
    .to_string()
}

fn normalize_runtime_tool_arguments(arguments: &Value) -> Value {
    let Some(raw) = arguments.as_object() else {
        return json!({});
    };
    let mut normalized: Map<String, Value> = raw.clone();
    if !normalized.contains_key("command") {
        if let Some(cmd) = raw.get("cmd") {
            normalized.insert("command".to_string(), cmd.clone());
        }
    }
    Value::Object(normalized)
}

fn runtime_tool_direct_call_hint(name: &str) -> Option<&'static str> {
    let normalized = name.trim();
    match normalized {
        "ls" => Some(r#"Call ls directly with {"path":"."}."#),
        "read_file" => Some(r#"Call read_file directly with {"path":"relative/path"}."#),
        "write_file" => {
            Some(r#"Call write_file directly with {"path":"relative/path","content":"..."}."#)
        }
        "edit_file" => Some(
            r#"Call edit_file directly with {"path":"relative/path","old_string":"...","new_string":"..."}."#,
        ),
        "glob" => Some(r#"Call glob directly with {"pattern":"**/*.go"}."#),
        "grep" => Some(r#"Call grep directly with {"pattern":"text","path":"."}."#),
        "execute" => Some(r#"Call execute directly with {"command":"..."}."#),
        "update_plan" => Some(r#"Call update_plan directly with {"items":[...]}."#),
        "todowrite" => Some(r#"Call todowrite directly with {"todos":[...]}."#),
        "runtime_echo" => Some(r#"Call runtime_echo directly with {"message":"..."}."#),
        "skill" => Some(r#"Call skill directly with {"name":"skill-name"}."#),
        "knowledge_search" => Some(r#"Call knowledge_search directly with {"query":"..."}."#),
        _ => None,
    }
}

fn runtime_tool_direct_call_message(name: &str) -> String {
    let hint = runtime_tool_direct_call_hint(name).unwrap_or("Call this runtime tool directly.");
    format!(
        "{name} is a CyberStrikeAI Agent Runtime tool, not an MCP tool. Do not call it through mcp_call. {hint}"
    )
}

fn update_plan_spec() -> ToolSpec {
    ToolSpec {
        name: "update_plan".to_string(),
        description: "Codex-compatible plan updater. Exactly one item may be in_progress. Do not finish the turn while any item is pending or in_progress.".to_string(),
        parameters: plan_parameters_schema(),
    }
}

fn todowrite_spec() -> ToolSpec {
    ToolSpec {
        name: "todowrite".to_string(),
        description: "OpenCode-compatible todo writer. Equivalent to update_plan.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": plan_item_schema()
                }
            },
            "required": ["todos"]
        }),
    }
}

fn runtime_echo_spec() -> ToolSpec {
    ToolSpec {
        name: "runtime_echo".to_string(),
        description: "Diagnostic echo tool for runtime plumbing tests.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "message": {"type": "string"}
            },
            "required": ["message"]
        }),
    }
}

fn skill_spec() -> ToolSpec {
    ToolSpec {
        name: "skill".to_string(),
        description: "Load a named skill instruction into the current turn as a tool result. The result includes SKILL.md, the skill base directory, a sampled package file list, optional resource files, and optional ripgrep-style searches over the skill package.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "skill_name": {"type": "string"},
                "id": {"type": "string"},
                "resources": {
                    "type": "array",
                    "description": "Optional relative paths under the loaded skill package to read after SKILL.md references them.",
                    "items": {"type": "string"}
                },
                "grep": {
                    "type": "string",
                    "description": "Optional regex/text pattern to search inside files under the skill package, backed by ripgrep when available."
                },
                "search": {
                    "type": "string",
                    "description": "Alias for grep."
                },
                "include": {
                    "type": "string",
                    "description": "Optional file glob passed to ripgrep for grep/search."
                },
                "path": {
                    "type": "string",
                    "description": "Optional relative file or directory path under the skill package to search."
                }
            }
        }),
    }
}

fn mcp_call_spec() -> ToolSpec {
    ToolSpec {
        name: "mcp_call".to_string(),
        description: "Compatibility fallback for older MCP calls. For Rust-owned MCP tools, use tool_search first and then call the loaded concrete tool schema.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "tool": {"type": "string"},
                "name": {"type": "string"},
                "tool_name": {"type": "string"},
                "arguments": {"type": "object"},
                "args": {"type": "object"}
            }
        }),
    }
}

fn tool_search_spec() -> ToolSpec {
    ToolSpec {
        name: "tool_search".to_string(),
        description: "Search the deferred CyberStrikeAI MCP tool catalog and optionally select one exact tool to load its full schema on the next model request. Use query for discovery, or select/select:<tool> for exact schema loading. Do not call a deferred MCP tool until its full schema is visible.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query, or select:<exact_tool> to load an exact tool."
                },
                "select": {
                    "type": "string",
                    "description": "Optional exact identity, model_tool_name, or tool name to load into the session."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20
                }
            },
            "required": ["query"]
        }),
    }
}

fn knowledge_search_spec() -> ToolSpec {
    ToolSpec {
        name: "knowledge_search".to_string(),
        description:
            "Search knowledge snippets injected by the Go adapter for this CyberStrikeAI turn."
                .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "top_k": {"type": "integer", "minimum": 1, "maximum": 10}
            },
            "required": ["query"]
        }),
    }
}

fn plan_parameters_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "items": plan_item_schema()
            },
            "plan": {
                "type": "array",
                "items": plan_item_schema()
            }
        }
    })
}

fn builtin_invocation(model_tool_name: &str) -> ToolInvocation {
    ToolInvocation {
        display_name: model_tool_name.to_string(),
        permission_name: model_tool_name.to_string(),
        permission_aliases: vec![model_tool_name.to_string()],
        requires_approval: false,
    }
}

fn unique_tool_name(base: &str, used_names: &mut Vec<String>) -> String {
    let mut candidate = base.to_string();
    let mut suffix = 2;
    while used_names.iter().any(|used| used == &candidate) {
        let suffix_text = format!("_{suffix}");
        let max_base_len = MODEL_TOOL_NAME_MAX_LEN
            .saturating_sub(suffix_text.len())
            .max(1);
        candidate = format!(
            "{}{}",
            truncate_tool_name_prefix(base, max_base_len),
            suffix_text
        );
        suffix += 1;
    }
    used_names.push(candidate.clone());
    candidate
}

fn truncate_tool_name_prefix(value: &str, max_len: usize) -> String {
    if value.len() <= max_len {
        return value.to_string();
    }
    value.chars().take(max_len).collect()
}

fn plan_item_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {"type": "string"},
            "step": {"type": "string"},
            "content": {"type": "string"},
            "status": {
                "type": "string",
                "enum": ["pending", "in_progress", "completed", "cancelled"]
            },
            "priority": {
                "type": "string",
                "description": "Priority level of the task: high, medium, low",
                "enum": ["high", "medium", "low"]
            }
        },
        "required": ["status"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem_runtime::FilesystemRuntime;
    use crate::mcp_registry::{McpLoadedTools, McpRegistryTool, McpToolRegistry};
    use crate::model_stream::{ModelToolCall, ModelToolFunction};
    use serde_json::json;

    #[test]
    fn builtin_registry_exposes_expected_tools() {
        let schemas = ToolRegistry::builtin().schemas();
        let text = schemas.to_string();
        assert!(text.contains("update_plan"));
        assert!(text.contains("todowrite"));
        assert!(text.contains("priority"));
        assert!(text.contains("runtime_echo"));
        assert!(text.contains("skill"));
        assert!(text.contains("mcp_call"));
        assert!(text.contains("tool_search"));
        assert!(text.contains("knowledge_search"));
    }

    #[test]
    fn registry_executes_plan_tool() {
        let registry = ToolRegistry::builtin();
        let call = ModelToolCall {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: ModelToolFunction {
                name: "update_plan".to_string(),
                arguments: json!({
                    "items": [
                        {"id": "a", "step": "A", "status": "completed"}
                    ]
                })
                .to_string(),
            },
        };
        let mut plan = PlanStore::default();
        let skills = SkillRuntime::default();
        let mcp = McpBridge::default();
        let mcp_registry = McpToolRegistry::new(Vec::new());
        let mut loaded = McpLoadedTools::default();
        let knowledge = KnowledgeRuntime::default();
        let filesystem = FilesystemRuntime::default();
        let mut ctx = ToolExecutionContext {
            plan: &mut plan,
            skills: &skills,
            mcp: &mcp,
            mcp_registry: &mcp_registry,
            mcp_loaded: &mut loaded,
            knowledge: &knowledge,
            filesystem: &filesystem,
            tool_timeout_seconds: 120,
        };
        let outcome = registry.execute(&call, &mut ctx, None).unwrap();
        assert!(matches!(outcome, ToolOutcome::PlanUpdated(_)));
        assert!(!plan.has_active_work());
    }

    #[test]
    fn registry_executes_skill_tool() {
        let registry = ToolRegistry::builtin();
        let call = ModelToolCall {
            id: "call_skill".to_string(),
            call_type: "function".to_string(),
            function: ModelToolFunction {
                name: "skill".to_string(),
                arguments: json!({"name": "demo"}).to_string(),
            },
        };
        let mut plan = PlanStore::default();
        let mut skill_context = serde_json::Map::new();
        skill_context.insert("skills".to_string(), json!({"demo": "Demo skill body"}));
        let skills = SkillRuntime::from_context(&skill_context);
        let mcp = McpBridge::default();
        let mcp_registry = McpToolRegistry::new(Vec::new());
        let mut loaded = McpLoadedTools::default();
        let knowledge = KnowledgeRuntime::default();
        let filesystem = FilesystemRuntime::default();
        let mut ctx = ToolExecutionContext {
            plan: &mut plan,
            skills: &skills,
            mcp: &mcp,
            mcp_registry: &mcp_registry,
            mcp_loaded: &mut loaded,
            knowledge: &knowledge,
            filesystem: &filesystem,
            tool_timeout_seconds: 120,
        };
        let outcome = registry.execute(&call, &mut ctx, None).unwrap();
        match outcome {
            ToolOutcome::Text(text) => assert!(text.contains("Demo skill body")),
            other => panic!("unexpected outcome: {:?}", other),
        }
    }

    fn registry_tool_from_json(value: Value) -> McpRegistryTool {
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("lookup")
            .to_string();
        let context = serde_json::Map::from_iter([
            ("mcp_enabled".to_string(), json!(true)),
            ("mcp_tools".to_string(), json!([value])),
        ]);
        McpToolRegistry::from_context(&context)
            .find(&name)
            .unwrap()
            .clone()
    }

    #[test]
    fn registry_exposes_only_loaded_mcp_tools_as_first_class_schemas() {
        let filesystem = FilesystemRuntime::default();
        let loaded = vec![registry_tool_from_json(json!({
            "server": "demo",
            "name": "lookup",
            "description": "Lookup demo data",
            "input_schema": {
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]
            },
            "enabled": true,
            "requires_approval": true
        }))];
        let registry = ToolRegistry::from_capabilities(&filesystem, &loaded);
        let schemas = registry.schemas();
        let text = schemas.to_string();

        assert!(text.contains("mcp__demo__lookup"));
        assert!(text.contains("Lookup demo data"));

        let call = ModelToolCall {
            id: "call_mcp".to_string(),
            call_type: "function".to_string(),
            function: ModelToolFunction {
                name: "mcp__demo__lookup".to_string(),
                arguments: json!({"query": "x"}).to_string(),
            },
        };
        let invocation = registry.invocation(&call);
        assert_eq!(invocation.display_name, "demo::lookup");
        assert_eq!(invocation.permission_name, "demo::lookup");
        assert!(invocation.requires_approval);
        assert!(invocation
            .permission_aliases
            .contains(&"mcp__demo__lookup".to_string()));
        assert!(!invocation
            .permission_aliases
            .contains(&"mcp_call".to_string()));
    }

    #[test]
    fn registry_routes_direct_mcp_tool_to_bridge() {
        let mcp_registry = McpToolRegistry::new(vec![registry_tool_from_json(json!({
            "server": "demo",
            "name": "lookup",
            "enabled": true
        }))]);
        let mut loaded = McpLoadedTools::default();
        loaded.mark_loaded(mcp_registry.find("demo::lookup").unwrap());
        let loaded_tools = vec![mcp_registry.find("demo::lookup").unwrap().clone()];
        let mcp = McpBridge::default();
        let filesystem = FilesystemRuntime::default();
        let registry = ToolRegistry::from_capabilities(&filesystem, &loaded_tools);
        let call = ModelToolCall {
            id: "call_mcp".to_string(),
            call_type: "function".to_string(),
            function: ModelToolFunction {
                name: "mcp__demo__lookup".to_string(),
                arguments: json!({"query": "x"}).to_string(),
            },
        };
        let mut plan = PlanStore::default();
        let skills = SkillRuntime::default();
        let knowledge = KnowledgeRuntime::default();
        let filesystem = FilesystemRuntime::default();
        let mut ctx = ToolExecutionContext {
            plan: &mut plan,
            skills: &skills,
            mcp: &mcp,
            mcp_registry: &mcp_registry,
            mcp_loaded: &mut loaded,
            knowledge: &knowledge,
            filesystem: &filesystem,
            tool_timeout_seconds: 120,
        };

        let err = registry.execute(&call, &mut ctx, None).unwrap_err();
        assert!(err
            .to_string()
            .contains("mcp endpoint URL is not configured"));
    }

    #[test]
    fn registry_executes_builtin_local_mcp_tool_without_endpoint() {
        let input_schema = json!({"type":"object","properties":{"message":{"type":"string"}}});
        let local_tool = McpRegistryTool {
            source: "builtin".to_string(),
            identity: "builtin::echo-local".to_string(),
            model_tool_name: "echo-local".to_string(),
            name: "echo-local".to_string(),
            call_name: "echo-local".to_string(),
            short_description: "local echo".to_string(),
            description: "local echo".to_string(),
            input_schema: input_schema.clone(),
            schema_token_estimate: 10,
            enabled: true,
            requires_approval: false,
            tags: vec!["test".to_string()],
            search_text: "echo-local".to_string(),
            schema_hash: "hash".to_string(),
            parameter_names: vec!["message".to_string()],
            local_executor: Some(crate::mcp_registry::LocalToolSpec {
                command: "sh".to_string(),
                base_args: vec![
                    "-c".to_string(),
                    "printf \"$1\"".to_string(),
                    "sh".to_string(),
                ],
                allowed_exit_codes: Vec::new(),
                parameters: vec![crate::mcp_registry::LocalToolParameter {
                    name: "message".to_string(),
                    r#type: "string".to_string(),
                    required: true,
                    default: None,
                    flag: None,
                    format: "positional".to_string(),
                    template: None,
                    position: Some(1),
                }],
            }),
        };
        let mcp_registry = McpToolRegistry::new(vec![local_tool]);
        let mut loaded = McpLoadedTools::default();
        let source_tool = mcp_registry.find("builtin::echo-local").unwrap().clone();
        loaded.mark_loaded(&source_tool);
        let loaded_tools = vec![source_tool];
        let registry =
            ToolRegistry::from_capabilities(&FilesystemRuntime::default(), &loaded_tools);
        let call = ModelToolCall {
            id: "call_local".to_string(),
            call_type: "function".to_string(),
            function: ModelToolFunction {
                name: "echo-local".to_string(),
                arguments: json!({"message":"local-ok"}).to_string(),
            },
        };
        let mut plan = PlanStore::default();
        let skills = SkillRuntime::default();
        let mcp = McpBridge::default();
        let knowledge = KnowledgeRuntime::default();
        let filesystem = FilesystemRuntime::default();
        let mut ctx = ToolExecutionContext {
            plan: &mut plan,
            skills: &skills,
            mcp: &mcp,
            mcp_registry: &mcp_registry,
            mcp_loaded: &mut loaded,
            knowledge: &knowledge,
            filesystem: &filesystem,
            tool_timeout_seconds: 5,
        };

        let outcome = registry.execute(&call, &mut ctx, None).unwrap();

        match outcome {
            ToolOutcome::Text(text) => {
                assert!(text.contains("\"tool_kind\":\"mcp\""), "{text}");
                assert!(text.contains("local-ok"), "{text}");
                assert!(text.contains("\"status\":\"completed\""), "{text}");
            }
            other => panic!("unexpected outcome: {:?}", other),
        }
    }

    #[test]
    fn registry_exposes_and_executes_filesystem_tools() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-tool-registry-fs-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("README.md"), "hello runtime").unwrap();
        let mut fs_context = serde_json::Map::new();
        fs_context.insert("filesystem_enabled".to_string(), json!(true));
        fs_context.insert(
            "workspace_root".to_string(),
            json!(root.to_string_lossy().to_string()),
        );
        let filesystem = FilesystemRuntime::from_context(&fs_context);
        let mcp = McpBridge::default();
        let mcp_registry = McpToolRegistry::new(Vec::new());
        let mut loaded = McpLoadedTools::default();
        let registry = ToolRegistry::from_capabilities(&filesystem, &[]);
        let schemas = registry.schemas().to_string();
        assert!(schemas.contains("\"name\":\"read_file\""));
        assert!(schemas.contains("\"name\":\"execute\""));

        let call = ModelToolCall {
            id: "call_read_file".to_string(),
            call_type: "function".to_string(),
            function: ModelToolFunction {
                name: "read_file".to_string(),
                arguments: json!({"path": "README.md"}).to_string(),
            },
        };
        let mut plan = PlanStore::default();
        let skills = SkillRuntime::default();
        let knowledge = KnowledgeRuntime::default();
        let mut ctx = ToolExecutionContext {
            plan: &mut plan,
            skills: &skills,
            mcp: &mcp,
            mcp_registry: &mcp_registry,
            mcp_loaded: &mut loaded,
            knowledge: &knowledge,
            filesystem: &filesystem,
            tool_timeout_seconds: 120,
        };
        let outcome = registry.execute(&call, &mut ctx, None).unwrap();
        match outcome {
            ToolOutcome::Text(text) => assert!(text.contains("hello runtime")),
            other => panic!("unexpected outcome: {:?}", other),
        }
        let invocation = registry.invocation(&call);
        assert_eq!(invocation.display_name, "read_file");
        assert!(!invocation.requires_approval);

        let write_call = ModelToolCall {
            id: "call_write_file".to_string(),
            call_type: "function".to_string(),
            function: ModelToolFunction {
                name: "write_file".to_string(),
                arguments: json!({"path": "out.txt", "content": "x"}).to_string(),
            },
        };
        let write_invocation = registry.invocation(&write_call);
        assert!(write_invocation.requires_approval);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mcp_call_rejects_unregistered_runtime_tool_with_direct_call_hint() {
        let mcp = McpBridge::default();
        let filesystem = FilesystemRuntime::default();
        let mcp_registry = McpToolRegistry::new(Vec::new());
        let mut loaded = McpLoadedTools::default();
        let registry = ToolRegistry::from_capabilities(&filesystem, &[]);
        let call = ModelToolCall {
            id: "call_execute_as_mcp".to_string(),
            call_type: "function".to_string(),
            function: ModelToolFunction {
                name: "mcp_call".to_string(),
                arguments: json!({
                    "tool_name": "execute",
                    "arguments": {
                        "command": "curl https://example.com"
                    }
                })
                .to_string(),
            },
        };
        let mut plan = PlanStore::default();
        let skills = SkillRuntime::default();
        let knowledge = KnowledgeRuntime::default();
        let mut ctx = ToolExecutionContext {
            plan: &mut plan,
            skills: &skills,
            mcp: &mcp,
            mcp_registry: &mcp_registry,
            mcp_loaded: &mut loaded,
            knowledge: &knowledge,
            filesystem: &filesystem,
            tool_timeout_seconds: 120,
        };

        let err = registry.execute(&call, &mut ctx, None).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("not an MCP tool"), "{text}");
        assert!(text.contains("Call execute directly"), "{text}");
        assert!(!text.contains("not found or disabled"), "{text}");
    }

    #[test]
    fn mcp_call_to_registered_runtime_tool_is_canonicalized_before_permission_and_execution() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-tool-registry-mcp-canonical-fs-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut fs_context = serde_json::Map::new();
        fs_context.insert("filesystem_enabled".to_string(), json!(true));
        fs_context.insert(
            "workspace_root".to_string(),
            json!(root.to_string_lossy().to_string()),
        );
        let filesystem = FilesystemRuntime::from_context(&fs_context);
        let mcp = McpBridge::default();
        let mcp_registry = McpToolRegistry::new(Vec::new());
        let mut loaded = McpLoadedTools::default();
        let registry = ToolRegistry::from_capabilities(&filesystem, &[]);
        let call = ModelToolCall {
            id: "call_execute_as_mcp".to_string(),
            call_type: "function".to_string(),
            function: ModelToolFunction {
                name: "mcp_call".to_string(),
                arguments: json!({
                    "tool_name": "execute",
                    "arguments": {
                        "cmd": "printf canonical"
                    }
                })
                .to_string(),
            },
        };

        let invocation = registry.invocation(&call);
        assert_eq!(invocation.display_name, "execute");
        assert_eq!(invocation.permission_name, "execute");
        assert!(invocation.requires_approval);
        assert!(invocation
            .permission_aliases
            .contains(&"mcp_call".to_string()));

        let mut plan = PlanStore::default();
        let skills = SkillRuntime::default();
        let knowledge = KnowledgeRuntime::default();
        let mut ctx = ToolExecutionContext {
            plan: &mut plan,
            skills: &skills,
            mcp: &mcp,
            mcp_registry: &mcp_registry,
            mcp_loaded: &mut loaded,
            knowledge: &knowledge,
            filesystem: &filesystem,
            tool_timeout_seconds: 120,
        };

        let outcome = registry.execute(&call, &mut ctx, None).unwrap();
        match outcome {
            ToolOutcome::Text(text) => {
                assert!(text.contains("\"tool\":\"execute\""), "{text}");
                assert!(text.contains("canonical"), "{text}");
            }
            other => panic!("unexpected outcome: {:?}", other),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn registry_exposes_builtin_mcp_tools_by_original_name() {
        let loaded = vec![registry_tool_from_json(json!({
            "server": "builtin",
            "name": "read_file",
            "call_name": "read_file",
            "model_name": "read_file",
            "enabled": true,
            "requires_approval": false
        }))];
        let filesystem = FilesystemRuntime::default();
        let registry = ToolRegistry::from_capabilities(&filesystem, &loaded);
        let schemas = registry.schemas();
        let text = schemas.to_string();
        assert!(text.contains("\"name\":\"read_file\""));
        assert!(!text.contains("mcp__builtin__read_file"));

        let call = ModelToolCall {
            id: "call_read".to_string(),
            call_type: "function".to_string(),
            function: ModelToolFunction {
                name: "read_file".to_string(),
                arguments: json!({"path": "README.md"}).to_string(),
            },
        };
        let invocation = registry.invocation(&call);
        assert_eq!(invocation.display_name, "builtin::read_file");
        assert!(invocation
            .permission_aliases
            .contains(&"read_file".to_string()));
    }

    #[test]
    fn registry_bounds_long_mcp_model_tool_names() {
        let loaded = vec![registry_tool_from_json(json!({
            "server": "very-long-server-name-with-many-segments-and-extra-context",
            "name": "very-long-tool-name-that-would-otherwise-exceed-openai-function-name-limit",
            "enabled": true
        }))];
        let filesystem = FilesystemRuntime::default();
        let registry = ToolRegistry::from_capabilities(&filesystem, &loaded);
        let schemas = registry.schemas();
        let tool_name = schemas
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
            })
            .find(|name| name.starts_with("mcp__"))
            .unwrap();

        assert!(tool_name.len() <= MODEL_TOOL_NAME_MAX_LEN, "{tool_name}");
        assert!(tool_name.contains("__h"));
    }

    #[test]
    fn registry_keeps_collision_suffix_within_model_tool_name_limit() {
        let tool = registry_tool_from_json(json!({
            "server": "very-long-server-name-with-many-segments-and-extra-context",
            "name": "very-long-tool-name-that-would-otherwise-exceed-openai-function-name-limit",
            "enabled": true
        }));
        let loaded = vec![tool.clone(), tool];
        let filesystem = FilesystemRuntime::default();
        let registry = ToolRegistry::from_capabilities(&filesystem, &loaded);
        let schemas = registry.schemas();
        let tool_names: Vec<&str> = schemas
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
            })
            .filter(|name| name.starts_with("mcp__"))
            .collect();

        assert_eq!(tool_names.len(), 2);
        assert!(tool_names
            .iter()
            .all(|name| name.len() <= MODEL_TOOL_NAME_MAX_LEN));
        assert_ne!(tool_names[0], tool_names[1]);
    }

    #[test]
    fn resolved_mcp_call_omits_generic_mcp_call_permission_alias() {
        let loaded = vec![registry_tool_from_json(json!({
            "server": "demo",
            "name": "lookup",
            "enabled": true,
            "requires_approval": false
        }))];
        let filesystem = FilesystemRuntime::default();
        let registry = ToolRegistry::from_capabilities(&filesystem, &loaded);
        let call = ModelToolCall {
            id: "call_mcp".to_string(),
            call_type: "function".to_string(),
            function: ModelToolFunction {
                name: "mcp_call".to_string(),
                arguments: json!({"tool":"demo::lookup","arguments":{"query":"x"}}).to_string(),
            },
        };

        let invocation = registry.invocation(&call);

        assert_eq!(invocation.display_name, "demo::lookup");
        assert!(!invocation.requires_approval);
        assert!(!invocation
            .permission_aliases
            .contains(&"mcp_call".to_string()));
        assert!(invocation
            .permission_aliases
            .contains(&"mcp__demo__lookup".to_string()));
    }
}
