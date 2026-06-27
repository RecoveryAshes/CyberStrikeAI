use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;

use crate::mcp_bridge::{bounded_model_tool_name, normalize_input_schema, MODEL_TOOL_NAME_MAX_LEN};
use crate::model_stream::ChatMessage;

#[derive(Debug, Clone)]
pub struct McpToolRegistry {
    tools: Vec<McpRegistryTool>,
    by_model_name: HashMap<String, usize>,
}

#[derive(Debug, Clone)]
pub struct McpRegistryTool {
    pub source: String,
    pub identity: String,
    pub model_tool_name: String,
    pub name: String,
    pub call_name: String,
    pub short_description: String,
    pub description: String,
    pub input_schema: Value,
    pub schema_token_estimate: usize,
    pub enabled: bool,
    pub requires_approval: bool,
    pub tags: Vec<String>,
    pub search_text: String,
    pub schema_hash: String,
    pub parameter_names: Vec<String>,
    pub local_executor: Option<LocalToolSpec>,
}

#[derive(Debug, Clone)]
pub struct McpLoadedTools {
    entries: HashMap<String, LoadedToolRecord>,
    clock: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadedToolRecord {
    pub identity: String,
    pub state: LoadedToolStatus,
    pub selected_at: u64,
    pub last_used_at: u64,
    pub used_count: u64,
    pub schema_hash: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadedToolStatus {
    SelectedPending,
    Loaded,
    RecentlyUsed,
    BudgetBlocked,
}

#[derive(Debug, Clone)]
pub struct McpLoadedSnapshot {
    pub records: Vec<LoadedToolRecord>,
}

#[derive(Debug, Clone, Default)]
pub struct McpBudgetPreferences {
    pub always_visible_tools: Vec<String>,
    pub relevance_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalToolSpec {
    pub command: String,
    pub base_args: Vec<String>,
    pub parameters: Vec<LocalToolParameter>,
    pub allowed_exit_codes: Vec<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalToolParameter {
    pub name: String,
    pub r#type: String,
    pub required: bool,
    pub default: Option<Value>,
    pub flag: Option<String>,
    pub format: String,
    pub template: Option<String>,
    pub position: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalToolCommand {
    pub program: String,
    pub args: Vec<String>,
    pub workdir: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolSearchOutput {
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct BudgetConfig {
    pub context_window_tokens: usize,
    pub output_reserve_tokens: usize,
    pub safety_margin_tokens: usize,
    pub tool_budget_ratio: f64,
    pub max_tool_schema_tokens: usize,
}

#[derive(Debug, Clone)]
pub struct SchemaBudgetReport {
    pub context_window_tokens: usize,
    pub messages_tokens: usize,
    pub catalog_tokens: usize,
    pub already_loaded_schema_tokens: usize,
    pub output_reserve_tokens: usize,
    pub safety_margin_tokens: usize,
    pub available_tokens: usize,
    pub tool_schema_budget: usize,
    pub loaded_tools: Vec<String>,
    pub selected_pending_tools: Vec<String>,
    pub budget_blocked_tools: Vec<String>,
    pub dropped_tools: Vec<String>,
    pub overloaded_selected_tools: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LoadedToolSchemas {
    pub schemas: Vec<McpRegistryTool>,
    pub report: SchemaBudgetReport,
}

#[derive(Debug, Clone)]
pub struct CompactCatalog {
    pub prompt: String,
    pub token_estimate: usize,
    pub count: usize,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub tool: McpRegistryTool,
    pub score: f64,
}

#[derive(Debug, Deserialize)]
struct RawToolFile {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    short_description: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    parameters: Vec<RawToolParameter>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    requires_approval: Option<bool>,
    #[serde(default)]
    #[serde(rename = "requiresApproval")]
    requires_approval_camel: Option<bool>,
    #[serde(default)]
    allowed_exit_codes: Vec<i32>,
}

#[derive(Debug, Deserialize)]
struct RawToolParameter {
    name: String,
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    default: Option<Value>,
    #[serde(default)]
    enum_values: Option<Vec<Value>>,
    #[serde(default)]
    #[serde(rename = "enum")]
    enum_: Option<Vec<Value>>,
    #[serde(default)]
    flag: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    position: Option<i64>,
}

#[derive(Debug, Error)]
pub enum LocalToolError {
    #[error("local MCP tool {0} is not registered with a Rust executor")]
    MissingExecutor(String),
    #[error("internal local MCP tool {0} is not implemented in Rust Agent Runtime")]
    InternalToolUnsupported(String),
    #[error("missing required parameter '{parameter}' for local MCP tool {tool}")]
    MissingRequired { tool: String, parameter: String },
    #[error("parameter '{parameter}' for local MCP tool {tool} must be an object")]
    InvalidObjectParameter { tool: String, parameter: String },
    #[error("local MCP tool {tool} has an empty command")]
    EmptyCommand { tool: String },
    #[error("run local MCP tool {tool}: {source}")]
    Io { tool: String, source: io::Error },
}

impl Default for McpLoadedTools {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            clock: now_unix(),
        }
    }
}

impl McpToolRegistry {
    pub fn from_context(context: &Map<String, Value>) -> Self {
        let mut tools = Vec::new();
        let role_allowlist = role_tool_allowlist(context);
        if let Some(local) = load_local_tools_from_context(context, &role_allowlist) {
            tools.extend(local);
        }
        if tools.is_empty() {
            tools.extend(load_context_tools(context, &role_allowlist));
        }
        Self::new(tools)
    }

    pub fn new(mut tools: Vec<McpRegistryTool>) -> Self {
        let mut used = HashSet::new();
        for tool in tools.iter_mut() {
            let unique = unique_model_tool_name(&tool.model_tool_name, &mut used);
            tool.model_tool_name = unique;
        }
        let by_model_name = tools
            .iter()
            .enumerate()
            .map(|(idx, tool)| (tool.model_tool_name.clone(), idx))
            .collect();
        Self {
            tools,
            by_model_name,
        }
    }

    pub fn enabled_count(&self) -> usize {
        self.tools.iter().filter(|tool| tool.enabled).count()
    }

    pub fn compact_catalog(&self) -> CompactCatalog {
        let entries = self
            .tools
            .iter()
            .filter(|tool| tool.enabled)
            .map(|tool| {
                json!({
                    "identity": tool.identity,
                    "model_tool_name": tool.model_tool_name,
                    "short_description": tool.short_description,
                    "parameter_names": tool.parameter_names,
                    "tags": tool.tags,
                })
            })
            .collect::<Vec<_>>();
        let catalog_json = serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string());
        let prompt = format!(
            "MCP_DEFERRED_TOOL_CATALOG:\n\
             The following compact catalog is only a name index. It intentionally omits full JSON schemas.\n\
             Do not guess arguments for MCP tools whose full schema is not currently visible in the tools array.\n\
             When you need an MCP tool, call tool_search first. Use query for discovery or select:<exact_tool> to load a known tool.\n\
             After tool_search selects a tool, the complete OpenAI-compatible function schema is provided on the next model request, then you may call it.\n\
             Compact catalog JSON:\n{}\n",
            catalog_json
        );
        let token_estimate = estimate_tokens(&prompt);
        CompactCatalog {
            prompt,
            token_estimate,
            count: entries.len(),
        }
    }

    pub fn search_tool(&self, arguments: &str, loaded: &mut McpLoadedTools) -> ToolSearchOutput {
        let args = serde_json::from_str::<Value>(arguments).unwrap_or_else(|_| json!({}));
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let select_arg = args
            .get("select")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(8)
            .clamp(1, 20) as usize;

        let select = select_arg.or_else(|| query.strip_prefix("select:").map(str::trim));
        let mut selected_identities = Vec::new();
        let results = if let Some(select) = select.filter(|value| !value.is_empty()) {
            if let Some(tool) = self.find(select) {
                loaded.mark_selected(tool);
                selected_identities.push(tool.identity.clone());
                vec![SearchResult {
                    tool: tool.clone(),
                    score: 1000.0,
                }]
            } else {
                Vec::new()
            }
        } else {
            self.search(query, limit)
        };

        let payload = json!({
            "tool": "tool_search",
            "query": query,
            "selected": selected_identities,
            "results": results.iter().map(|result| {
                json!({
                    "identity": result.tool.identity,
                    "model_tool_name": result.tool.model_tool_name,
                    "name": result.tool.name,
                    "short_description": result.tool.short_description,
                    "parameter_names": result.tool.parameter_names,
                    "tags": result.tool.tags,
                    "loaded_state": loaded.state_label(&result.tool.identity).unwrap_or("not_loaded"),
                    "score": result.score,
                    "schema_token_estimate": result.tool.schema_token_estimate,
                })
            }).collect::<Vec<_>>(),
            "instruction": "Full JSON schemas are not included in this result. If you selected a tool, wait for the next model request before calling it; do not guess parameters."
        });
        ToolSearchOutput {
            content: payload.to_string(),
        }
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchResult> {
        let query_terms = tokenize(query);
        let mut results = self
            .tools
            .iter()
            .filter(|tool| tool.enabled)
            .filter_map(|tool| {
                let mut score = exact_match_score(tool, query);
                for term in &query_terms {
                    score += term_score(tool, term);
                }
                if score <= 0.0 {
                    None
                } else {
                    Some(SearchResult {
                        tool: tool.clone(),
                        score,
                    })
                }
            })
            .collect::<Vec<_>>();
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    a.tool
                        .schema_token_estimate
                        .cmp(&b.tool.schema_token_estimate)
                })
                .then_with(|| a.tool.identity.cmp(&b.tool.identity))
        });
        results.truncate(limit);
        results
    }

    pub fn find(&self, requested: &str) -> Option<&McpRegistryTool> {
        let requested = requested.trim();
        if requested.is_empty() {
            return None;
        }
        if let Some(index) = self.by_model_name.get(requested) {
            return self.tools.get(*index).filter(|tool| tool.enabled);
        }
        self.tools
            .iter()
            .find(|tool| tool.enabled && tool.matches(requested))
    }

    pub fn schemas_for_request(
        &self,
        messages: &[ChatMessage],
        loaded: &mut McpLoadedTools,
        catalog_tokens: usize,
        config: &BudgetConfig,
        preferences: &McpBudgetPreferences,
    ) -> LoadedToolSchemas {
        loaded.reconcile(self);
        let messages_tokens = estimate_messages_tokens(messages);
        let candidates = self.budget_candidates(loaded, preferences);
        let mut candidates = candidates
            .into_iter()
            .filter_map(|candidate| {
                self.find(&candidate.identity)
                    .map(|tool| (tool.clone(), candidate))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| {
            let ap = a.1.priority;
            let bp = b.1.priority;
            bp.partial_cmp(&ap)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.schema_token_estimate.cmp(&b.0.schema_token_estimate))
                .then_with(|| a.0.identity.cmp(&b.0.identity))
        });

        let fixed = messages_tokens
            .saturating_add(catalog_tokens)
            .saturating_add(config.output_reserve_tokens)
            .saturating_add(config.safety_margin_tokens);
        let available_tokens = config.context_window_tokens.saturating_sub(fixed);
        let ratio_budget = (available_tokens as f64 * config.tool_budget_ratio).floor() as usize;
        let tool_schema_budget = config.max_tool_schema_tokens.min(ratio_budget);
        let mut used = 0usize;
        let mut schemas = Vec::new();
        let mut loaded_tools = Vec::new();
        let mut selected_pending_tools = Vec::new();
        let mut budget_blocked_tools = Vec::new();
        let mut dropped_tools = Vec::new();
        let mut overloaded_selected_tools = Vec::new();

        for (tool, candidate) in candidates {
            if used.saturating_add(tool.schema_token_estimate) <= tool_schema_budget {
                used = used.saturating_add(tool.schema_token_estimate);
                loaded_tools.push(tool.identity.clone());
                loaded.mark_loaded(&tool);
                schemas.push(tool);
            } else {
                let selected = loaded.is_selected_or_loaded(&tool.identity);
                if selected && tool.schema_token_estimate > tool_schema_budget {
                    overloaded_selected_tools.push(tool.identity.clone());
                }
                if selected {
                    loaded.mark_budget_blocked(&tool);
                    budget_blocked_tools.push(tool.identity.clone());
                    selected_pending_tools.push(tool.identity.clone());
                } else {
                    loaded.drop_autoload_candidate(&tool.identity);
                    dropped_tools.push(tool.identity.clone());
                }
                if candidate.source == CandidateSource::AlwaysVisible {
                    dropped_tools.push(tool.identity.clone());
                }
            }
        }
        loaded.prune_stale_loaded(&loaded_tools);
        let mut selected_pending_tools = loaded.selected_pending_identities();
        selected_pending_tools.sort();
        let mut budget_blocked_tools = loaded.budget_blocked_identities();
        budget_blocked_tools.sort();

        LoadedToolSchemas {
            schemas,
            report: SchemaBudgetReport {
                context_window_tokens: config.context_window_tokens,
                messages_tokens,
                catalog_tokens,
                already_loaded_schema_tokens: used,
                output_reserve_tokens: config.output_reserve_tokens,
                safety_margin_tokens: config.safety_margin_tokens,
                available_tokens,
                tool_schema_budget,
                loaded_tools,
                selected_pending_tools,
                budget_blocked_tools,
                dropped_tools,
                overloaded_selected_tools,
            },
        }
    }

    pub fn execute_local_tool(
        &self,
        identity: &str,
        arguments: Value,
        timeout_seconds: u64,
    ) -> Result<Value, LocalToolError> {
        let Some(tool) = self.find(identity) else {
            return Err(LocalToolError::MissingExecutor(identity.to_string()));
        };
        let command = tool.build_local_command(arguments)?;
        run_local_command(tool, command, timeout_seconds.max(1))
    }

    fn budget_candidates(
        &self,
        loaded: &mut McpLoadedTools,
        preferences: &McpBudgetPreferences,
    ) -> Vec<BudgetCandidate> {
        let mut candidates: HashMap<String, BudgetCandidate> = HashMap::new();
        for requested in &preferences.always_visible_tools {
            if let Some(tool) = self.find(requested) {
                add_candidate(
                    &mut candidates,
                    &tool.identity,
                    CandidateSource::AlwaysVisible,
                    10_000.0 + loaded.priority(&tool.identity),
                );
            }
        }
        for record in loaded.records() {
            let base = match record.state {
                LoadedToolStatus::BudgetBlocked => 9_000.0,
                LoadedToolStatus::SelectedPending => 8_000.0,
                LoadedToolStatus::Loaded => 6_000.0,
                LoadedToolStatus::RecentlyUsed => 7_000.0,
            };
            add_candidate(
                &mut candidates,
                &record.identity,
                CandidateSource::LoadedState,
                base + loaded.priority(&record.identity),
            );
        }
        if !preferences.relevance_text.trim().is_empty() {
            for result in self.search(&preferences.relevance_text, 6) {
                add_candidate(
                    &mut candidates,
                    &result.tool.identity,
                    CandidateSource::AutoRelevant,
                    result.score,
                );
            }
        }
        candidates.into_values().collect()
    }
}

impl McpRegistryTool {
    pub fn matches(&self, requested: &str) -> bool {
        let requested = requested.trim();
        requested == self.identity
            || requested == self.name
            || requested == self.model_tool_name
            || requested == self.call_name
            || requested == format!("{}::{}", self.source, self.name)
    }

    pub fn is_local_builtin(&self) -> bool {
        self.source == "builtin" && self.local_executor.is_some()
    }

    pub fn build_local_command(
        &self,
        arguments: Value,
    ) -> Result<LocalToolCommand, LocalToolError> {
        let Some(spec) = self.local_executor.as_ref() else {
            return Err(LocalToolError::MissingExecutor(self.identity.clone()));
        };
        if spec.command.trim().is_empty() {
            return Err(LocalToolError::EmptyCommand {
                tool: self.identity.clone(),
            });
        }
        if spec.command.trim().starts_with("internal:") {
            return Err(LocalToolError::InternalToolUnsupported(
                self.identity.clone(),
            ));
        }
        let args = build_local_command_args(self, spec, &arguments)?;
        let workdir = if self.name == "exec" {
            arguments
                .get("workdir")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        } else {
            None
        };
        Ok(LocalToolCommand {
            program: local_program(self, spec, &arguments),
            args,
            workdir,
        })
    }
}

impl McpLoadedTools {
    pub fn contains(&self, identity: &str) -> bool {
        self.entries.get(identity).is_some_and(|entry| {
            matches!(
                entry.state,
                LoadedToolStatus::Loaded | LoadedToolStatus::RecentlyUsed
            )
        })
    }

    pub fn mark_selected(&mut self, tool: &McpRegistryTool) {
        self.clock = self.clock.saturating_add(1);
        let now = self.clock;
        let entry = self.entry_for(tool);
        entry.selected_at = now;
        entry.last_used_at = now;
        if !matches!(
            entry.state,
            LoadedToolStatus::Loaded | LoadedToolStatus::RecentlyUsed
        ) {
            entry.state = LoadedToolStatus::SelectedPending;
        }
    }

    pub fn mark_loaded(&mut self, tool: &McpRegistryTool) {
        self.clock = self.clock.saturating_add(1);
        let now = self.clock;
        let entry = self.entry_for(tool);
        if entry.selected_at == 0 {
            entry.selected_at = now;
        }
        entry.last_used_at = now;
        entry.schema_hash = tool.schema_hash.clone();
        entry.state = LoadedToolStatus::Loaded;
    }

    pub fn mark_budget_blocked(&mut self, tool: &McpRegistryTool) {
        self.clock = self.clock.saturating_add(1);
        let now = self.clock;
        let entry = self.entry_for(tool);
        if entry.selected_at == 0 {
            entry.selected_at = now;
        }
        entry.last_used_at = now;
        entry.schema_hash = tool.schema_hash.clone();
        entry.state = LoadedToolStatus::BudgetBlocked;
    }

    pub fn mark_used(&mut self, tool: &McpRegistryTool) {
        self.clock = self.clock.saturating_add(1);
        let now = self.clock;
        let entry = self.entry_for(tool);
        if entry.selected_at == 0 {
            entry.selected_at = now;
        }
        entry.last_used_at = now;
        entry.schema_hash = tool.schema_hash.clone();
        entry.used_count = entry.used_count.saturating_add(1);
        entry.state = LoadedToolStatus::RecentlyUsed;
    }

    pub fn state_label(&self, identity: &str) -> Option<&'static str> {
        self.entries.get(identity).map(|entry| match entry.state {
            LoadedToolStatus::SelectedPending => "selected_pending",
            LoadedToolStatus::Loaded => "loaded",
            LoadedToolStatus::RecentlyUsed => "recently_used",
            LoadedToolStatus::BudgetBlocked => "budget_blocked",
        })
    }

    pub fn snapshot(&self) -> McpLoadedSnapshot {
        let mut records = self.records();
        records.sort_by(|a, b| a.identity.cmp(&b.identity));
        McpLoadedSnapshot { records }
    }

    pub fn from_records(registry: &McpToolRegistry, records: Vec<LoadedToolRecord>) -> Self {
        let mut loaded = Self::default();
        for record in records {
            if let Some(tool) = registry.find(&record.identity) {
                if record.schema_hash == tool.schema_hash {
                    loaded.clock = loaded
                        .clock
                        .max(record.selected_at)
                        .max(record.last_used_at);
                    loaded.entries.insert(record.identity.clone(), record);
                }
            }
        }
        loaded
    }

    pub fn reconcile(&mut self, registry: &McpToolRegistry) {
        self.entries.retain(|identity, record| {
            registry
                .find(identity)
                .is_some_and(|tool| tool.schema_hash == record.schema_hash)
        });
    }

    fn entry_for(&mut self, tool: &McpRegistryTool) -> &mut LoadedToolRecord {
        self.entries
            .entry(tool.identity.clone())
            .or_insert_with(|| LoadedToolRecord {
                identity: tool.identity.clone(),
                state: LoadedToolStatus::SelectedPending,
                selected_at: 0,
                last_used_at: 0,
                used_count: 0,
                schema_hash: tool.schema_hash.clone(),
            })
    }

    fn records(&self) -> Vec<LoadedToolRecord> {
        self.entries.values().cloned().collect()
    }

    fn is_selected_or_loaded(&self, identity: &str) -> bool {
        self.entries.contains_key(identity)
    }

    fn selected_pending_identities(&self) -> Vec<String> {
        self.entries
            .values()
            .filter(|entry| {
                matches!(
                    entry.state,
                    LoadedToolStatus::SelectedPending | LoadedToolStatus::BudgetBlocked
                )
            })
            .map(|entry| entry.identity.clone())
            .collect()
    }

    fn budget_blocked_identities(&self) -> Vec<String> {
        self.entries
            .values()
            .filter(|entry| entry.state == LoadedToolStatus::BudgetBlocked)
            .map(|entry| entry.identity.clone())
            .collect()
    }

    fn drop_autoload_candidate(&mut self, identity: &str) {
        if self.entries.get(identity).is_some_and(|entry| {
            matches!(
                entry.state,
                LoadedToolStatus::Loaded | LoadedToolStatus::RecentlyUsed
            )
        }) {
            self.entries.remove(identity);
        }
    }

    fn prune_stale_loaded(&mut self, loaded_identities: &[String]) {
        let loaded = loaded_identities.iter().collect::<HashSet<_>>();
        for entry in self.entries.values_mut() {
            if matches!(
                entry.state,
                LoadedToolStatus::Loaded | LoadedToolStatus::RecentlyUsed
            ) && !loaded.contains(&entry.identity)
            {
                entry.state = LoadedToolStatus::SelectedPending;
            }
        }
    }

    fn priority(&self, identity: &str) -> f64 {
        self.entries
            .get(identity)
            .map(|entry| {
                entry.selected_at as f64
                    + (entry.last_used_at as f64 * 2.0)
                    + (entry.used_count as f64 * 10.0)
            })
            .unwrap_or(0.0)
    }
}

impl BudgetConfig {
    pub fn from_context(context: &Map<String, Value>) -> Self {
        let context_window_tokens = context_usize(context, "context_window_tokens")
            .or_else(|| context_usize(context, "openai_context_window_tokens"))
            .unwrap_or_else(|| default_context_window(context));
        let output_reserve_tokens = context_usize(context, "output_reserve_tokens").unwrap_or(8192);
        let safety_margin_tokens = context_usize(context, "safety_margin_tokens").unwrap_or(4096);
        let tool_budget_ratio = context_f64(context, "tool_budget_ratio").unwrap_or(0.15);
        let max_tool_schema_tokens = context_usize(context, "max_tool_schema_tokens")
            .unwrap_or_else(|| {
                ((context_window_tokens as f64) * 0.25).floor().max(8192.0) as usize
            });
        Self {
            context_window_tokens,
            output_reserve_tokens,
            safety_margin_tokens,
            tool_budget_ratio: tool_budget_ratio.clamp(0.01, 0.5),
            max_tool_schema_tokens,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateSource {
    AlwaysVisible,
    LoadedState,
    AutoRelevant,
}

#[derive(Debug, Clone)]
struct BudgetCandidate {
    identity: String,
    source: CandidateSource,
    priority: f64,
}

fn add_candidate(
    candidates: &mut HashMap<String, BudgetCandidate>,
    identity: &str,
    source: CandidateSource,
    priority: f64,
) {
    candidates
        .entry(identity.to_string())
        .and_modify(|candidate| {
            if priority > candidate.priority {
                candidate.priority = priority;
                candidate.source = source;
            }
        })
        .or_insert_with(|| BudgetCandidate {
            identity: identity.to_string(),
            source,
            priority,
        });
}

impl McpBudgetPreferences {
    pub fn from_context(context: &Map<String, Value>, messages: &[ChatMessage]) -> Self {
        let always_visible_tools = context_string_list(context, "mcp_always_visible_tools")
            .or_else(|| context_string_list(context, "always_visible_tools"))
            .unwrap_or_default();
        let mut relevance_parts = Vec::new();
        if let Some(user) = messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .and_then(|message| message.content.as_deref())
        {
            relevance_parts.push(user.to_string());
        }
        if let Some(plan) = context.get("current_plan").and_then(Value::as_str) {
            relevance_parts.push(plan.to_string());
        }
        Self {
            always_visible_tools,
            relevance_text: relevance_parts.join("\n"),
        }
    }
}

fn load_local_tools_from_context(
    context: &Map<String, Value>,
    role_allowlist: &Option<HashSet<String>>,
) -> Option<Vec<McpRegistryTool>> {
    let tools_dir = context
        .get("mcp_tools_dir")
        .or_else(|| context.get("tools_dir"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            context
                .get("workspace_root")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|root| PathBuf::from(root).join("tools"))
        })?;
    Some(load_local_tools(&tools_dir, role_allowlist))
}

fn load_local_tools(
    tools_dir: &Path,
    role_allowlist: &Option<HashSet<String>>,
) -> Vec<McpRegistryTool> {
    let Ok(entries) = fs::read_dir(tools_dir) else {
        return Vec::new();
    };
    let mut tools = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|value| value.to_str()) else {
            continue;
        };
        if ext != "yaml" && ext != "yml" {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(file) = serde_yaml::from_str::<RawToolFile>(&raw) else {
            continue;
        };
        if let Some(tool) = registry_tool_from_file(file, raw, role_allowlist) {
            tools.push(tool);
        }
    }
    tools.sort_by(|a, b| a.identity.cmp(&b.identity));
    tools
}

fn load_context_tools(
    context: &Map<String, Value>,
    role_allowlist: &Option<HashSet<String>>,
) -> Vec<McpRegistryTool> {
    context
        .get("mcp_tools")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| registry_tool_from_context(item, role_allowlist))
                .collect()
        })
        .unwrap_or_default()
}

fn registry_tool_from_file(
    file: RawToolFile,
    raw: String,
    role_allowlist: &Option<HashSet<String>>,
) -> Option<McpRegistryTool> {
    let name = file.name.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let source = "builtin".to_string();
    let identity = format!("{source}::{name}");
    let model_tool_name = sanitize_model_name(&name);
    if !role_allows(role_allowlist, &source, &name, &identity, &model_tool_name) {
        return None;
    }
    let parameter_names = file
        .parameters
        .iter()
        .map(|parameter| parameter.name.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let input_schema = normalize_input_schema(parameters_to_schema(&file.parameters));
    let description = file.description.trim().to_string();
    let short_description = first_non_empty([&file.short_description, &description, &name]);
    let tags = if file.tags.is_empty() {
        infer_tags(&name, &description)
    } else {
        clean_list(file.tags)
    };
    let schema_hash = stable_hash_hex(&input_schema.to_string());
    let local_executor = LocalToolSpec {
        command: file.command.trim().to_string(),
        base_args: file.args.clone(),
        parameters: file
            .parameters
            .iter()
            .map(local_parameter_from_raw)
            .collect(),
        allowed_exit_codes: file.allowed_exit_codes.clone(),
    };
    let search_text = build_search_text(&[
        &source,
        &identity,
        &model_tool_name,
        &name,
        &short_description,
        &description,
        &parameter_names.join(" "),
        &tags.join(" "),
    ]);
    let schema_token_estimate = estimate_tokens(&input_schema.to_string());
    Some(McpRegistryTool {
        source,
        identity,
        model_tool_name,
        name: name.clone(),
        call_name: name,
        short_description,
        description,
        input_schema,
        schema_token_estimate,
        enabled: file.enabled.unwrap_or(true),
        requires_approval: file
            .requires_approval
            .or(file.requires_approval_camel)
            .unwrap_or_else(|| tool_requires_approval(&raw)),
        tags,
        search_text,
        schema_hash,
        parameter_names,
        local_executor: Some(local_executor),
    })
}

fn registry_tool_from_context(
    value: &Value,
    role_allowlist: &Option<HashSet<String>>,
) -> Option<McpRegistryTool> {
    let obj = value.as_object()?;
    let source = obj
        .get("server")
        .or_else(|| obj.get("external_mcp"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let name = obj
        .get("name")
        .or_else(|| obj.get("tool"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let identity = format!("{source}::{name}");
    let model_tool_name = obj
        .get("model_name")
        .or_else(|| obj.get("modelName"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(sanitize_model_name)
        .unwrap_or_else(|| bounded_model_tool_name(&source, &name));
    if !role_allows(role_allowlist, &source, &name, &identity, &model_tool_name) {
        return None;
    }
    let input_schema = normalize_input_schema(
        obj.get("input_schema")
            .or_else(|| obj.get("inputSchema"))
            .cloned()
            .unwrap_or_else(|| json!({"type":"object","properties":{}})),
    );
    let parameter_names = schema_parameter_names(&input_schema);
    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let short_description = obj
        .get("short_description")
        .or_else(|| obj.get("shortDescription"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let short_description = first_non_empty([&short_description, &description, &name]);
    let tags = obj
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| infer_tags(&name, &description));
    let schema_hash = stable_hash_hex(&input_schema.to_string());
    let search_text = build_search_text(&[
        &source,
        &identity,
        &model_tool_name,
        &name,
        &short_description,
        &description,
        &parameter_names.join(" "),
        &tags.join(" "),
    ]);
    Some(McpRegistryTool {
        source,
        identity,
        model_tool_name,
        name: name.clone(),
        call_name: obj
            .get("call_name")
            .or_else(|| obj.get("callName"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&name)
            .to_string(),
        short_description,
        description,
        schema_token_estimate: estimate_tokens(&input_schema.to_string()),
        input_schema,
        enabled: obj.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        requires_approval: obj
            .get("requires_approval")
            .or_else(|| obj.get("requiresApproval"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        tags,
        search_text,
        schema_hash,
        parameter_names,
        local_executor: None,
    })
}

fn parameters_to_schema(parameters: &[RawToolParameter]) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for parameter in parameters {
        let name = parameter.name.trim();
        if name.is_empty() {
            continue;
        }
        let mut schema = Map::new();
        schema.insert(
            "type".to_string(),
            Value::String(json_schema_type(&parameter.r#type).to_string()),
        );
        if !parameter.description.trim().is_empty() {
            schema.insert(
                "description".to_string(),
                Value::String(parameter.description.trim().to_string()),
            );
        }
        if let Some(default) = &parameter.default {
            schema.insert("default".to_string(), default.clone());
        }
        if let Some(values) = parameter.enum_values.as_ref().or(parameter.enum_.as_ref()) {
            schema.insert("enum".to_string(), Value::Array(values.clone()));
        }
        if parameter.required {
            required.push(Value::String(name.to_string()));
        }
        properties.insert(name.to_string(), Value::Object(schema));
    }
    let mut root = Map::new();
    root.insert("type".to_string(), Value::String("object".to_string()));
    root.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        root.insert("required".to_string(), Value::Array(required));
    }
    Value::Object(root)
}

fn local_parameter_from_raw(parameter: &RawToolParameter) -> LocalToolParameter {
    LocalToolParameter {
        name: parameter.name.trim().to_string(),
        r#type: parameter.r#type.trim().to_string(),
        required: parameter.required,
        default: parameter.default.clone(),
        flag: parameter
            .flag
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        format: parameter
            .format
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("flag")
            .to_string(),
        template: parameter
            .template
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        position: parameter.position,
    }
}

fn build_local_command_args(
    tool: &McpRegistryTool,
    spec: &LocalToolSpec,
    arguments: &Value,
) -> Result<Vec<String>, LocalToolError> {
    let args_obj = arguments
        .as_object()
        .ok_or_else(|| LocalToolError::InvalidObjectParameter {
            tool: tool.identity.clone(),
            parameter: "arguments".to_string(),
        })?;
    if tool.name == "exec" {
        let command = args_obj
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| LocalToolError::MissingRequired {
                tool: tool.identity.clone(),
                parameter: "command".to_string(),
            })?;
        return Ok(vec!["-c".to_string(), command.to_string()]);
    }

    let mut cmd_args = Vec::new();
    let scan_type_value = args_obj
        .get("scan_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if !(tool.name == "nmap" && scan_type_value.is_some()) {
        cmd_args.extend(spec.base_args.clone());
    }

    let mut positional = spec
        .parameters
        .iter()
        .filter(|parameter| parameter.position.is_some())
        .collect::<Vec<_>>();
    positional.sort_by_key(|parameter| parameter.position.unwrap_or(i64::MAX));
    let flag_params = spec
        .parameters
        .iter()
        .filter(|parameter| parameter.position.is_none())
        .collect::<Vec<_>>();

    if let Some(position_zero) = positional
        .iter()
        .copied()
        .find(|parameter| parameter.position == Some(0) && !is_special_local_parameter(parameter))
    {
        if let Some(value) = local_parameter_value(args_obj, position_zero) {
            push_formatted_parameter(&mut cmd_args, position_zero, &value);
        } else if position_zero.required {
            return Err(LocalToolError::MissingRequired {
                tool: tool.identity.clone(),
                parameter: position_zero.name.clone(),
            });
        }
    }

    for parameter in flag_params {
        if is_special_local_parameter(parameter) {
            continue;
        }
        let Some(value) = local_parameter_value(args_obj, parameter) else {
            if parameter.required {
                return Err(LocalToolError::MissingRequired {
                    tool: tool.identity.clone(),
                    parameter: parameter.name.clone(),
                });
            }
            continue;
        };
        if is_empty_command_value(&value) {
            if parameter.required {
                return Err(LocalToolError::MissingRequired {
                    tool: tool.identity.clone(),
                    parameter: parameter.name.clone(),
                });
            }
            continue;
        }
        push_formatted_parameter(&mut cmd_args, parameter, &value);
    }

    for parameter in positional {
        if parameter.position == Some(0) || is_special_local_parameter(parameter) {
            continue;
        }
        let Some(value) = local_parameter_value(args_obj, parameter) else {
            if parameter.required {
                return Err(LocalToolError::MissingRequired {
                    tool: tool.identity.clone(),
                    parameter: parameter.name.clone(),
                });
            }
            continue;
        };
        if is_empty_command_value(&value) {
            if parameter.required {
                return Err(LocalToolError::MissingRequired {
                    tool: tool.identity.clone(),
                    parameter: parameter.name.clone(),
                });
            }
            continue;
        }
        cmd_args.push(format_local_value(parameter, &value));
    }

    if let Some(scan_type) = scan_type_value {
        let scan_args = split_additional_args(scan_type);
        if !scan_args.is_empty() {
            let insert_pos = cmd_args
                .iter()
                .rposition(|arg| !arg.starts_with('-'))
                .unwrap_or(cmd_args.len());
            cmd_args.splice(insert_pos..insert_pos, scan_args);
        }
    }

    if let Some(additional) = args_obj
        .get("additional_args")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        cmd_args.extend(split_additional_args(additional));
    }

    if cmd_args.is_empty() && !spec.parameters.is_empty() {
        return Err(LocalToolError::MissingRequired {
            tool: tool.identity.clone(),
            parameter: "arguments".to_string(),
        });
    }
    Ok(cmd_args)
}

fn local_program(tool: &McpRegistryTool, spec: &LocalToolSpec, arguments: &Value) -> String {
    if tool.name == "exec" {
        return arguments
            .get("shell")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("sh")
            .to_string();
    }
    spec.command.clone()
}

fn local_parameter_value(
    args: &Map<String, Value>,
    parameter: &LocalToolParameter,
) -> Option<Value> {
    args.get(&parameter.name)
        .cloned()
        .or_else(|| parameter.default.clone())
}

fn is_special_local_parameter(parameter: &LocalToolParameter) -> bool {
    matches!(
        parameter.name.as_str(),
        "additional_args" | "scan_type" | "action" | "shell" | "workdir"
    )
}

fn push_formatted_parameter(out: &mut Vec<String>, parameter: &LocalToolParameter, value: &Value) {
    if is_bool_type(parameter) {
        if !value_as_bool(value) {
            return;
        }
        if let Some(flag) = parameter.flag.as_deref().filter(|flag| !flag.is_empty()) {
            out.push(flag.to_string());
        }
        return;
    }

    match parameter.format.as_str() {
        "combined" => {
            let formatted = format_local_value(parameter, value);
            if let Some(flag) = parameter.flag.as_deref().filter(|flag| !flag.is_empty()) {
                out.push(format!("{flag}={formatted}"));
            } else if !formatted.is_empty() {
                out.push(formatted);
            }
        }
        "template" => {
            if let Some(template) = parameter.template.as_deref() {
                let rendered = template
                    .replace("{flag}", parameter.flag.as_deref().unwrap_or_default())
                    .replace("{value}", &format_local_value(parameter, value))
                    .replace("{name}", &parameter.name);
                out.extend(split_additional_args(&rendered));
            } else {
                if let Some(flag) = parameter.flag.as_deref().filter(|flag| !flag.is_empty()) {
                    out.push(flag.to_string());
                }
                let formatted = format_local_value(parameter, value);
                if !formatted.is_empty() {
                    out.push(formatted);
                }
            }
        }
        "positional" => {
            let formatted = format_local_value(parameter, value);
            if !formatted.is_empty() {
                out.push(formatted);
            }
        }
        _ => {
            if let Some(flag) = parameter.flag.as_deref().filter(|flag| !flag.is_empty()) {
                out.push(flag.to_string());
            }
            let formatted = format_local_value(parameter, value);
            if !formatted.is_empty() {
                out.push(formatted);
            }
        }
    }
}

fn is_bool_type(parameter: &LocalToolParameter) -> bool {
    matches!(parameter.r#type.as_str(), "bool" | "boolean")
}

fn value_as_bool(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_i64().unwrap_or(0) != 0,
        Value::String(text) => matches!(
            text.trim().to_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        _ => false,
    }
}

fn is_empty_command_value(value: &Value) -> bool {
    value.as_str().is_some_and(|value| value.trim().is_empty())
}

fn format_local_value(parameter: &LocalToolParameter, value: &Value) -> String {
    match parameter.r#type.as_str() {
        "array" | "list" => value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(scalar_value_to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_else(|| scalar_value_to_string(value)),
        "object" | "map" => serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
        _ => {
            let mut formatted = scalar_value_to_string(value);
            if parameter.name == "ports" {
                formatted = formatted.replace(' ', "");
            }
            formatted
        }
    }
}

fn scalar_value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Null => String::new(),
        _ => serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
    }
}

fn split_additional_args(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }
        if ch.is_whitespace() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn run_local_command(
    tool: &McpRegistryTool,
    command: LocalToolCommand,
    timeout_seconds: u64,
) -> Result<Value, LocalToolError> {
    let mut cmd = Command::new(&command.program);
    cmd.args(&command.args);
    if let Some(workdir) = &command.workdir {
        cmd.current_dir(workdir);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("PYTHONUNBUFFERED", "1");
    cmd.env("PYTHONIOENCODING", "utf-8");
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|source| LocalToolError::Io {
        tool: tool.identity.clone(),
        source,
    })?;
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_seconds);
    let mut timed_out = false;
    loop {
        if child
            .try_wait()
            .map_err(|source| LocalToolError::Io {
                tool: tool.identity.clone(),
                source,
            })?
            .is_some()
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let output = child
        .wait_with_output()
        .map_err(|source| LocalToolError::Io {
            tool: tool.identity.clone(),
            source,
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = if stderr.trim().is_empty() {
        stdout.to_string()
    } else if stdout.trim().is_empty() {
        stderr.to_string()
    } else {
        format!("{stdout}\n{stderr}")
    };
    let exit_code = output.status.code().unwrap_or(-1);
    let ok = !timed_out
        && (output.status.success()
            || tool
                .local_executor
                .as_ref()
                .is_some_and(|spec| spec.allowed_exit_codes.contains(&exit_code)));
    let text = if timed_out {
        format!(
            "工具执行超时，已终止进程（{} 秒）。\n{}",
            timeout_seconds, combined
        )
    } else {
        combined
    };
    Ok(json!({
        "content": [{"type": "text", "text": truncate_tool_output(&text)}],
        "isError": !ok,
        "metadata": {
            "program": command.program,
            "args": command.args,
            "workdir": command.workdir,
            "exitCode": exit_code,
            "timeoutSeconds": timeout_seconds,
            "timedOut": timed_out
        }
    }))
}

fn truncate_tool_output(value: &str) -> String {
    const MAX_CHARS: usize = 200_000;
    let mut out = value.chars().take(MAX_CHARS).collect::<String>();
    if value.chars().count() > MAX_CHARS {
        out.push_str("\n[output truncated by Rust Agent Runtime]");
    }
    out
}

fn json_schema_type(value: &str) -> &'static str {
    match value.trim().to_lowercase().as_str() {
        "bool" | "boolean" => "boolean",
        "int" | "integer" => "integer",
        "float" | "number" => "number",
        "array" | "list" => "array",
        "object" | "map" => "object",
        _ => "string",
    }
}

fn schema_parameter_names(schema: &Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default()
}

fn role_tool_allowlist(context: &Map<String, Value>) -> Option<HashSet<String>> {
    let items = context
        .get("role_tools")
        .or_else(|| context.get("mcp_role_tools"))
        .and_then(Value::as_array)?;
    let set = items
        .iter()
        .filter_map(Value::as_str)
        .map(normalize_role_tool_key)
        .filter(|value| !value.is_empty())
        .collect::<HashSet<_>>();
    if set.is_empty() {
        None
    } else {
        Some(set)
    }
}

fn role_allows(
    allowlist: &Option<HashSet<String>>,
    source: &str,
    name: &str,
    identity: &str,
    model_tool_name: &str,
) -> bool {
    let Some(allowlist) = allowlist else {
        return true;
    };
    [
        name.to_string(),
        identity.to_string(),
        model_tool_name.to_string(),
        format!("{source}::{name}"),
    ]
    .iter()
    .map(|value| normalize_role_tool_key(value))
    .any(|key| allowlist.contains(&key))
}

fn normalize_role_tool_key(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("mcp:")
        .trim_start_matches("mcp::")
        .to_string()
}

fn sanitize_model_name(value: &str) -> String {
    let mut out = String::new();
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let out = out.trim_matches('_');
    let out = if out.is_empty() { "tool" } else { out };
    out.chars().take(MODEL_TOOL_NAME_MAX_LEN).collect()
}

fn unique_model_tool_name(base: &str, used: &mut HashSet<String>) -> String {
    let base = sanitize_model_name(base);
    if used.insert(base.clone()) {
        return base;
    }
    for suffix in 2.. {
        let suffix_text = format!("_{suffix}");
        let prefix_len = MODEL_TOOL_NAME_MAX_LEN
            .saturating_sub(suffix_text.len())
            .max(1);
        let candidate = format!(
            "{}{}",
            base.chars().take(prefix_len).collect::<String>(),
            suffix_text
        );
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!()
}

fn first_non_empty<const N: usize>(items: [&str; N]) -> String {
    items
        .iter()
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn clean_list(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        let item = item.trim().to_string();
        if !item.is_empty() && seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

fn infer_tags(name: &str, description: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let text = format!("{} {}", name, description).to_lowercase();
    for (needle, tag) in [
        ("web", "web"),
        ("http", "http"),
        ("scan", "scanner"),
        ("port", "network"),
        ("dns", "dns"),
        ("cloud", "cloud"),
        ("kube", "kubernetes"),
        ("binary", "binary"),
        ("cve", "vulnerability"),
    ] {
        if text.contains(needle) {
            tags.push(tag.to_string());
        }
    }
    if tags.is_empty() {
        tags.push("mcp".to_string());
    }
    clean_list(tags)
}

fn tool_requires_approval(raw: &str) -> bool {
    let text = raw.to_lowercase();
    text.contains("任意shell命令")
        || text.contains("execute arbitrary")
        || text.contains("安全风险")
        || text.contains("requires approval")
}

fn exact_match_score(tool: &McpRegistryTool, query: &str) -> f64 {
    let query = query.trim();
    if query.is_empty() {
        return 0.0;
    }
    if tool.matches(query) {
        100.0
    } else if tool.name.contains(query) || tool.identity.contains(query) {
        20.0
    } else {
        0.0
    }
}

fn term_score(tool: &McpRegistryTool, term: &str) -> f64 {
    let mut score = 0.0;
    if tool.name.to_lowercase().contains(term) {
        score += 12.0;
    }
    if tool.identity.to_lowercase().contains(term) {
        score += 10.0;
    }
    if tool.model_tool_name.to_lowercase().contains(term) {
        score += 8.0;
    }
    if tool.short_description.to_lowercase().contains(term) {
        score += 6.0;
    }
    if tool.description.to_lowercase().contains(term) {
        score += 3.0;
    }
    if tool
        .parameter_names
        .iter()
        .any(|name| name.to_lowercase().contains(term))
    {
        score += 4.0;
    }
    if tool
        .tags
        .iter()
        .any(|tag| tag.to_lowercase().contains(term))
    {
        score += 5.0;
    }
    if tool.search_text.contains(term) {
        score += 1.0;
    }
    score / (tool.schema_token_estimate as f64).sqrt().max(1.0)
}

fn tokenize(value: &str) -> Vec<String> {
    value
        .to_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "select")
        .map(ToOwned::to_owned)
        .collect()
}

fn build_search_text(parts: &[&str]) -> String {
    parts.join(" ").to_lowercase()
}

pub fn estimate_messages_tokens(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|message| estimate_tokens(&serde_json::to_string(message).unwrap_or_default()))
        .sum()
}

pub fn estimate_tokens(value: &str) -> usize {
    let ascii = value.chars().filter(|ch| ch.is_ascii()).count();
    let non_ascii = value.chars().count().saturating_sub(ascii);
    ((ascii + 3) / 4).saturating_add((non_ascii + 1) / 2).max(1)
}

fn context_usize(context: &Map<String, Value>, key: &str) -> Option<usize> {
    match context.get(key)? {
        Value::Number(number) => number.as_u64().map(|value| value as usize),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn context_string_list(context: &Map<String, Value>, key: &str) -> Option<Vec<String>> {
    let value = context.get(key)?;
    match value {
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect(),
        ),
        Value::String(text) => Some(
            text.split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect(),
        ),
        _ => None,
    }
}

fn context_f64(context: &Map<String, Value>, key: &str) -> Option<f64> {
    match context.get(key)? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn default_context_window(context: &Map<String, Value>) -> usize {
    let model = context
        .get("openai_model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    if model.contains("32k") {
        32_768
    } else if model.contains("16k") {
        16_384
    } else {
        128_000
    }
}

pub fn stable_hash_hex(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn loads_local_tools_and_filters_by_role() {
        let root = temp_dir();
        fs::create_dir_all(root.join("tools")).unwrap();
        fs::write(
            root.join("tools/nmap.yaml"),
            r#"
name: nmap
command: nmap
enabled: true
short_description: Port scanner
parameters:
  - name: target
    type: string
    required: true
"#,
        )
        .unwrap();
        fs::write(
            root.join("tools/huge.yaml"),
            r#"
name: huge-tool
command: huge-tool
enabled: true
short_description: Huge schema
parameters:
  - name: url
    type: string
"#,
        )
        .unwrap();
        let mut context = Map::new();
        context.insert(
            "workspace_root".to_string(),
            Value::String(root.to_string_lossy().to_string()),
        );
        context.insert("role_tools".to_string(), json!(["nmap"]));

        let registry = McpToolRegistry::from_context(&context);

        assert_eq!(registry.enabled_count(), 1);
        assert!(registry.find("nmap").is_some());
        assert!(registry.find("huge-tool").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tool_search_select_marks_loaded_without_returning_full_schema() {
        let registry = McpToolRegistry::new(vec![demo_tool("builtin", "nmap")]);
        let mut loaded = McpLoadedTools::default();

        let out = registry.search_tool(r#"{"query":"select:nmap"}"#, &mut loaded);

        assert_eq!(
            loaded.state_label("builtin::nmap"),
            Some("selected_pending")
        );
        assert!(out.content.contains("\"parameter_names\""));
        assert!(!out.content.contains("\"properties\""));
    }

    #[test]
    fn schema_budget_keeps_selected_tool_pending_when_context_is_too_full() {
        let tool = McpRegistryTool {
            input_schema: json!({"type":"object","properties":{"blob":{"type":"string","description":"x".repeat(800)}}}),
            schema_token_estimate: 1000,
            ..demo_tool("builtin", "huge")
        };
        let registry = McpToolRegistry::new(vec![tool]);
        let mut loaded = McpLoadedTools::default();
        loaded.mark_selected(registry.find("builtin::huge").unwrap());
        let schemas = registry.schemas_for_request(
            &[ChatMessage::user("x".repeat(1000))],
            &mut loaded,
            100,
            &BudgetConfig {
                context_window_tokens: 1200,
                output_reserve_tokens: 800,
                safety_margin_tokens: 200,
                tool_budget_ratio: 0.15,
                max_tool_schema_tokens: 5000,
            },
            &McpBudgetPreferences::default(),
        );

        assert!(schemas.schemas.is_empty());
        assert!(schemas
            .report
            .budget_blocked_tools
            .contains(&"builtin::huge".to_string()));
        assert!(schemas
            .report
            .selected_pending_tools
            .contains(&"builtin::huge".to_string()));
        assert_eq!(loaded.state_label("builtin::huge"), Some("budget_blocked"));
    }

    #[test]
    fn local_tool_command_maps_yaml_parameters() {
        let mut tool = demo_tool("builtin", "nmap");
        tool.local_executor = Some(LocalToolSpec {
            command: "nmap".to_string(),
            base_args: vec!["-sT".to_string(), "-sV".to_string(), "-sC".to_string()],
            allowed_exit_codes: Vec::new(),
            parameters: vec![
                LocalToolParameter {
                    name: "target".to_string(),
                    r#type: "string".to_string(),
                    required: true,
                    default: None,
                    flag: None,
                    format: "positional".to_string(),
                    template: None,
                    position: Some(1),
                },
                LocalToolParameter {
                    name: "ports".to_string(),
                    r#type: "string".to_string(),
                    required: false,
                    default: None,
                    flag: Some("-p".to_string()),
                    format: "flag".to_string(),
                    template: None,
                    position: None,
                },
                LocalToolParameter {
                    name: "timing".to_string(),
                    r#type: "string".to_string(),
                    required: false,
                    default: None,
                    flag: None,
                    format: "template".to_string(),
                    template: Some("-T{value}".to_string()),
                    position: None,
                },
                LocalToolParameter {
                    name: "os_detection".to_string(),
                    r#type: "bool".to_string(),
                    required: false,
                    default: Some(json!(false)),
                    flag: Some("-O".to_string()),
                    format: "flag".to_string(),
                    template: None,
                    position: None,
                },
                LocalToolParameter {
                    name: "scan_type".to_string(),
                    r#type: "string".to_string(),
                    required: false,
                    default: None,
                    flag: None,
                    format: "template".to_string(),
                    template: Some("{value}".to_string()),
                    position: None,
                },
                LocalToolParameter {
                    name: "additional_args".to_string(),
                    r#type: "string".to_string(),
                    required: false,
                    default: None,
                    flag: None,
                    format: "positional".to_string(),
                    template: None,
                    position: None,
                },
            ],
        });

        let command = tool
            .build_local_command(json!({
                "target": "example.com",
                "ports": "80,443, 8080",
                "timing": "4",
                "os_detection": true,
                "scan_type": "-sT -sV",
                "additional_args": "--max-retries 3"
            }))
            .unwrap();

        assert_eq!(command.program, "nmap");
        assert_eq!(
            command.args,
            vec![
                "-p",
                "80,443,8080",
                "-T4",
                "-O",
                "-sT",
                "-sV",
                "example.com",
                "--max-retries",
                "3"
            ]
        );
    }

    fn demo_tool(source: &str, name: &str) -> McpRegistryTool {
        let identity = format!("{source}::{name}");
        let input_schema = json!({"type":"object","properties":{"target":{"type":"string"}}});
        McpRegistryTool {
            source: source.to_string(),
            identity: identity.clone(),
            model_tool_name: sanitize_model_name(name),
            name: name.to_string(),
            call_name: name.to_string(),
            short_description: format!("{name} short"),
            description: format!("{name} long"),
            input_schema: input_schema.clone(),
            schema_token_estimate: estimate_tokens(&input_schema.to_string()),
            enabled: true,
            requires_approval: false,
            tags: vec!["scanner".to_string()],
            search_text: name.to_string(),
            schema_hash: stable_hash_hex(&input_schema.to_string()),
            parameter_names: vec!["target".to_string()],
            local_executor: None,
        }
    }

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("csai-mcp-registry-{nanos}"))
    }
}
