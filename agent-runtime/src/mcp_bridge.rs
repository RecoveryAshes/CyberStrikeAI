use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use serde_json::{json, Map, Value};
use std::time::Duration;
use thiserror::Error;

pub const MODEL_TOOL_NAME_MAX_LEN: usize = 64;

#[derive(Debug, Clone)]
pub struct McpBridge {
    tools: Vec<McpTool>,
    endpoint_url: Option<String>,
    auth_header: Option<(String, String)>,
    client: Client,
}

impl Default for McpBridge {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            endpoint_url: None,
            auth_header: None,
            client: Client::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    pub call_name: String,
    pub model_name: Option<String>,
    pub transport: String,
    pub description: String,
    pub input_schema: Value,
    pub enabled: bool,
    pub requires_approval: bool,
}

#[derive(Debug, Error)]
pub enum McpBridgeError {
    #[error("invalid mcp_call arguments: {0}")]
    InvalidArguments(#[from] serde_json::Error),
    #[error("mcp tool name is required")]
    MissingToolName,
    #[error("mcp tool not found or disabled: {0}")]
    ToolNotFound(String),
    #[error("mcp endpoint URL is not configured")]
    MissingEndpoint,
    #[error("invalid mcp auth header name: {0}")]
    InvalidAuthHeader(String),
    #[error("invalid mcp auth header value")]
    InvalidAuthHeaderValue,
    #[error("mcp HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("mcp HTTP request returned status {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("mcp JSON-RPC error {code}: {message}")]
    JsonRpc { code: i64, message: String },
    #[error("mcp JSON-RPC response missing result")]
    MissingResult,
}

impl McpBridge {
    pub fn from_context(context: &Map<String, Value>) -> Self {
        let enabled = context
            .get("mcp_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !enabled {
            return Self::default();
        }

        let tools = context
            .get("mcp_tools")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_mcp_tool).collect())
            .unwrap_or_default();
        let endpoint_url = context
            .get("mcp_endpoint_url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(ToOwned::to_owned);
        let auth_header_name = context
            .get("mcp_auth_header")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let auth_header_value = context
            .get("mcp_auth_header_value")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let auth_header = auth_header_name
            .zip(auth_header_value)
            .map(|(name, value)| (name.to_string(), value.to_string()));
        Self {
            tools,
            endpoint_url,
            auth_header,
            client: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    pub fn enabled_tool_count(&self) -> usize {
        self.tools.iter().filter(|tool| tool.enabled).count()
    }

    pub fn tool_summaries(&self) -> Vec<String> {
        self.tools
            .iter()
            .filter(|tool| tool.enabled)
            .map(|tool| {
                let full_name = tool.full_name();
                let schema = tool.input_schema.to_string();
                let label = if tool.description.trim().is_empty() {
                    full_name
                } else {
                    format!("{}: {}", full_name, tool.description.trim())
                };
                format!("{} | schema: {}", label, schema)
            })
            .collect()
    }

    pub fn enabled_tools(&self) -> Vec<McpTool> {
        self.tools
            .iter()
            .filter(|tool| tool.enabled)
            .cloned()
            .collect()
    }

    pub fn tool_description(&self, identity: &str) -> Option<String> {
        self.tools
            .iter()
            .find(|tool| tool.enabled && tool.matches(identity))
            .map(|tool| tool.description.trim().to_string())
    }

    pub fn tool_input_schema(&self, identity: &str) -> Option<Value> {
        self.tools
            .iter()
            .find(|tool| tool.enabled && tool.matches(identity))
            .map(|tool| normalize_input_schema(tool.input_schema.clone()))
    }

    pub fn execute_call(&self, arguments: &str) -> Result<String, McpBridgeError> {
        let args: Value = serde_json::from_str(arguments)?;
        let requested = args
            .get("tool")
            .or_else(|| args.get("name"))
            .or_else(|| args.get("tool_name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or(McpBridgeError::MissingToolName)?;
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.enabled && tool.matches(requested))
            .ok_or_else(|| McpBridgeError::ToolNotFound(requested.to_string()))?;
        let call_args = args
            .get("arguments")
            .or_else(|| args.get("args"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        self.execute_resolved_tool(tool, call_args, "mcp_call")
    }

    pub fn execute_tool(
        &self,
        identity: &str,
        arguments: Value,
        model_tool_name: &str,
    ) -> Result<String, McpBridgeError> {
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.enabled && tool.matches(identity))
            .ok_or_else(|| McpBridgeError::ToolNotFound(identity.to_string()))?;
        self.execute_resolved_tool(tool, arguments, model_tool_name)
    }

    fn execute_resolved_tool(
        &self,
        tool: &McpTool,
        arguments: Value,
        model_tool_name: &str,
    ) -> Result<String, McpBridgeError> {
        let result = self.call_mcp_tool(tool, arguments.clone())?;
        let status = if mcp_result_is_error(&result) {
            "failed"
        } else {
            "completed"
        };
        Ok(json!({
            "tool": model_tool_name,
            "tool_kind": "mcp",
            "mcp_tool": tool.full_name(),
            "server": tool.server,
            "name": tool.name,
            "arguments": arguments,
            "status": status,
            "result": result
        })
        .to_string())
    }

    fn call_mcp_tool(&self, tool: &McpTool, arguments: Value) -> Result<Value, McpBridgeError> {
        let endpoint = self
            .endpoint_url
            .as_deref()
            .ok_or(McpBridgeError::MissingEndpoint)?;
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some((name, value)) = &self.auth_header {
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| McpBridgeError::InvalidAuthHeader(name.clone()))?;
            let header_value =
                HeaderValue::from_str(value).map_err(|_| McpBridgeError::InvalidAuthHeaderValue)?;
            headers.insert(header_name, header_value);
        }

        let request = json!({
            "jsonrpc": "2.0",
            "id": "cyberstrike-agent-runtime-mcp-call",
            "method": "tools/call",
            "params": {
                "name": tool.call_name(),
                "arguments": arguments
            }
        });
        let response = self
            .client
            .post(endpoint)
            .headers(headers)
            .json(&request)
            .send()?;
        let status = response.status();
        let body = response.text()?;
        if !status.is_success() {
            return Err(McpBridgeError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }
        let response_value: Value = serde_json::from_str(&body)?;
        if let Some(error) = response_value.get("error") {
            return Err(McpBridgeError::JsonRpc {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown JSON-RPC error")
                    .to_string(),
            });
        }
        response_value
            .get("result")
            .cloned()
            .ok_or(McpBridgeError::MissingResult)
    }
}

impl McpTool {
    pub fn full_name(&self) -> String {
        format!("{}::{}", self.server, self.name)
    }

    pub fn call_name(&self) -> String {
        if self.transport == "builtin" || self.server == "builtin" {
            return self.name.clone();
        }
        let trimmed = self.call_name.trim();
        if trimmed.is_empty() {
            self.full_name()
        } else {
            trimmed.to_string()
        }
    }

    pub fn model_tool_name(&self) -> String {
        if let Some(model_name) = self.model_name.as_deref() {
            let trimmed = model_name.trim();
            if !trimmed.is_empty() && trimmed.len() <= MODEL_TOOL_NAME_MAX_LEN {
                return sanitize_tool_segment(trimmed);
            }
        }
        bounded_model_tool_name(&self.server, &self.name)
    }

    pub fn matches(&self, requested: &str) -> bool {
        requested == self.name
            || requested == self.full_name()
            || requested == self.call_name()
            || requested == self.model_tool_name()
    }
}

fn bounded_model_tool_name(server: &str, name: &str) -> String {
    let server = sanitize_tool_segment(server);
    let name = sanitize_tool_segment(name);
    let candidate = format!("mcp__{}__{}", server, name);
    if candidate.len() <= MODEL_TOOL_NAME_MAX_LEN {
        return candidate;
    }

    let hash = stable_hash_hex(&format!("{server}\0{name}"));
    let suffix = format!("__h{hash}");
    let fixed_len = "mcp__".len() + "__".len() + suffix.len();
    let budget = MODEL_TOOL_NAME_MAX_LEN.saturating_sub(fixed_len);
    let (server_budget, name_budget) = split_segment_budget(&server, &name, budget);
    format!(
        "mcp__{}__{}{}",
        truncate_ascii_segment(&server, server_budget),
        truncate_ascii_segment(&name, name_budget),
        suffix
    )
}

fn normalize_input_schema(schema: Value) -> Value {
    let mut obj = schema.as_object().cloned().unwrap_or_default();
    match obj.get("type").and_then(Value::as_str) {
        Some("object") => {}
        _ => {
            obj.insert("type".to_string(), Value::String("object".to_string()));
        }
    }
    obj.entry("properties".to_string())
        .or_insert_with(|| json!({}));
    Value::Object(obj)
}

fn sanitize_tool_segment(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let out = out.trim_matches('_');
    if out.is_empty() {
        "tool".to_string()
    } else {
        out.to_string()
    }
}

fn split_segment_budget(server: &str, name: &str, budget: usize) -> (usize, usize) {
    if budget <= 2 {
        return (1, 1);
    }
    let half = budget / 2;
    let mut server_budget = half.min(server.len()).max(1);
    let mut name_budget = (budget - server_budget).min(name.len()).max(1);
    if server.len() < half {
        name_budget = (budget - server.len()).min(name.len()).max(1);
    } else if name.len() < budget - half {
        server_budget = (budget - name.len()).min(server.len()).max(1);
    }
    if server_budget + name_budget > budget {
        name_budget = budget.saturating_sub(server_budget).max(1);
    }
    (server_budget, name_budget)
}

fn truncate_ascii_segment(value: &str, max_len: usize) -> String {
    let mut out = value.chars().take(max_len.max(1)).collect::<String>();
    out = out.trim_matches('_').to_string();
    if out.is_empty() {
        "tool".to_string()
    } else {
        out
    }
}

fn stable_hash_hex(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

pub fn wrapped_mcp_result_is_error(result: &str) -> bool {
    serde_json::from_str::<Value>(result)
        .ok()
        .is_some_and(|value| {
            value.get("tool_kind").and_then(Value::as_str) == Some("mcp")
                && value.get("status").and_then(Value::as_str) == Some("failed")
        })
}

fn mcp_result_is_error(result: &Value) -> bool {
    result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn parse_mcp_tool(value: &Value) -> Option<McpTool> {
    let obj = value.as_object()?;
    let server = obj
        .get("server")
        .or_else(|| obj.get("external_mcp"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let name = obj
        .get("name")
        .or_else(|| obj.get("tool"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let transport = obj
        .get("transport")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            if server == "builtin" {
                "builtin".to_string()
            } else {
                "external".to_string()
            }
        });
    Some(McpTool {
        server,
        name,
        call_name: obj
            .get("call_name")
            .or_else(|| obj.get("callName"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_default(),
        model_name: obj
            .get("model_name")
            .or_else(|| obj.get("modelName"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned),
        transport,
        description: obj
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        input_schema: obj
            .get("input_schema")
            .or_else(|| obj.get("inputSchema"))
            .cloned()
            .unwrap_or_else(|| json!({"type":"object","properties":{}})),
        enabled: obj.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        requires_approval: obj
            .get("requires_approval")
            .or_else(|| obj.get("requiresApproval"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn loads_enabled_tools_from_context_and_executes() {
        let mut context = Map::new();
        context.insert("mcp_enabled".to_string(), Value::Bool(true));
        context.insert(
            "mcp_tools".to_string(),
            json!([
                {
                    "server": "demo",
                    "name": "lookup",
                    "description": "Lookup demo data",
                    "enabled": true,
                    "requires_approval": false
                }
            ]),
        );
        let bridge = McpBridge::from_context(&context);
        assert_eq!(bridge.enabled_tool_count(), 1);
        assert!(bridge.tool_summaries()[0].contains("demo::lookup"));

        let err = bridge
            .execute_call(r#"{"tool":"demo::lookup","arguments":{"query":"x"}}"#)
            .unwrap_err();
        assert!(matches!(err, McpBridgeError::MissingEndpoint));
    }

    #[test]
    fn bridge_does_not_reject_approval_required_tools_after_runtime_policy() {
        let mut context = Map::new();
        context.insert("mcp_enabled".to_string(), Value::Bool(true));
        context.insert(
            "mcp_tools".to_string(),
            json!([
                {
                    "server": "demo",
                    "name": "write_file",
                    "enabled": true,
                    "requires_approval": true
                }
            ]),
        );
        let bridge = McpBridge::from_context(&context);
        let err = bridge
            .execute_call(r#"{"tool":"demo::write_file","arguments":{}}"#)
            .unwrap_err();
        assert!(matches!(err, McpBridgeError::MissingEndpoint));
    }

    #[test]
    fn executes_mcp_call_via_json_rpc_http() {
        let (url, received_rx) = start_mock_mcp_server(
            r#"{"jsonrpc":"2.0","id":"cyberstrike-agent-runtime-mcp-call","result":{"content":[{"type":"text","text":"lookup result"}]}}"#,
        );
        let mut context = Map::new();
        context.insert("mcp_enabled".to_string(), Value::Bool(true));
        context.insert("mcp_endpoint_url".to_string(), Value::String(url));
        context.insert(
            "mcp_auth_header".to_string(),
            Value::String("X-MCP-Token".to_string()),
        );
        context.insert(
            "mcp_auth_header_value".to_string(),
            Value::String("secret".to_string()),
        );
        context.insert(
            "mcp_tools".to_string(),
            json!([
                {
                    "server": "demo",
                    "name": "lookup",
                    "enabled": true,
                    "requires_approval": false
                }
            ]),
        );
        let bridge = McpBridge::from_context(&context);
        let result = bridge
            .execute_call(r#"{"tool":"demo::lookup","arguments":{"query":"x"}}"#)
            .unwrap();
        assert!(result.contains("\"status\":\"completed\""));
        assert!(result.contains("lookup result"));

        let received = received_rx.recv().unwrap();
        assert!(received.contains("POST /mcp HTTP/1.1"));
        assert!(received.contains("x-mcp-token: secret"));
        assert!(received.contains("\"method\":\"tools/call\""));
        assert!(received.contains("\"name\":\"demo::lookup\""));
    }

    #[test]
    fn builtin_mcp_tool_uses_call_name_and_model_name() {
        let (url, received_rx) = start_mock_mcp_server(
            r#"{"jsonrpc":"2.0","id":"cyberstrike-agent-runtime-mcp-call","result":{"content":[{"type":"text","text":"file body"}]}}"#,
        );
        let mut context = Map::new();
        context.insert("mcp_enabled".to_string(), Value::Bool(true));
        context.insert("mcp_endpoint_url".to_string(), Value::String(url));
        context.insert(
            "mcp_tools".to_string(),
            json!([
                {
                    "server": "builtin",
                    "name": "read_file",
                    "call_name": "read_file",
                    "model_name": "read_file",
                    "enabled": true
                }
            ]),
        );
        let bridge = McpBridge::from_context(&context);
        assert_eq!(bridge.enabled_tool_count(), 1);
        assert_eq!(bridge.enabled_tools()[0].model_tool_name(), "read_file");

        let result = bridge
            .execute_tool("read_file", json!({"path": "README.md"}), "read_file")
            .unwrap();
        assert!(result.contains("file body"));

        let received = received_rx.recv().unwrap();
        assert!(received.contains("\"name\":\"read_file\""));
        assert!(!received.contains("builtin::read_file"));
    }

    #[test]
    fn mcp_result_is_error_wraps_failed_status_without_losing_payload() {
        let (url, _received_rx) = start_mock_mcp_server(
            r#"{"jsonrpc":"2.0","id":"cyberstrike-agent-runtime-mcp-call","result":{"isError":true,"content":[{"type":"text","text":"lookup failed"}]}}"#,
        );
        let mut context = Map::new();
        context.insert("mcp_enabled".to_string(), Value::Bool(true));
        context.insert("mcp_endpoint_url".to_string(), Value::String(url));
        context.insert(
            "mcp_tools".to_string(),
            json!([
                {
                    "server": "demo",
                    "name": "lookup",
                    "enabled": true
                }
            ]),
        );
        let bridge = McpBridge::from_context(&context);

        let result = bridge
            .execute_call(r#"{"tool":"demo::lookup","arguments":{"query":"x"}}"#)
            .unwrap();

        assert!(result.contains("\"status\":\"failed\""));
        assert!(result.contains("\"isError\":true"));
        assert!(result.contains("lookup failed"));
        assert!(wrapped_mcp_result_is_error(&result));
    }

    #[test]
    fn surfaces_json_rpc_errors() {
        let (url, _received_rx) = start_mock_mcp_server(
            r#"{"jsonrpc":"2.0","id":"cyberstrike-agent-runtime-mcp-call","error":{"code":-32601,"message":"Tool not found"}}"#,
        );
        let mut context = Map::new();
        context.insert("mcp_enabled".to_string(), Value::Bool(true));
        context.insert("mcp_endpoint_url".to_string(), Value::String(url));
        context.insert(
            "mcp_tools".to_string(),
            json!([
                {
                    "server": "demo",
                    "name": "missing",
                    "enabled": true
                }
            ]),
        );
        let bridge = McpBridge::from_context(&context);
        let err = bridge
            .execute_call(r#"{"tool":"demo::missing","arguments":{}}"#)
            .unwrap_err();
        assert!(matches!(err, McpBridgeError::JsonRpc { code: -32601, .. }));
    }

    #[test]
    fn model_tool_name_is_bounded_and_hash_stable_for_long_names() {
        let tool = McpTool {
            server: "very-long-server-name-with-many-segments-and-extra-context".to_string(),
            name: "very-long-tool-name-that-would-otherwise-exceed-openai-function-name-limit"
                .to_string(),
            call_name: String::new(),
            model_name: None,
            transport: "external".to_string(),
            description: String::new(),
            input_schema: json!({"type":"object"}),
            enabled: true,
            requires_approval: false,
        };

        let model_name = tool.model_tool_name();

        assert!(model_name.len() <= MODEL_TOOL_NAME_MAX_LEN, "{model_name}");
        assert!(model_name.starts_with("mcp__"));
        assert!(model_name.contains("__h"));
        assert_eq!(model_name, tool.model_tool_name());
        assert!(tool.matches(&model_name));
    }

    fn start_mock_mcp_server(response_body: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 8192];
            let bytes = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            tx.send(request).unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{}/mcp", addr), rx)
    }
}
