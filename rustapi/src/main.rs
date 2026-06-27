use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use sqlx_postgres::{PgPool, PgPoolOptions};
use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio::{net::TcpListener, time};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

const FRONTEND_CONFIG_KEY: &str = "chat_web_frontend";

#[derive(Debug, Parser)]
#[command(name = "cyberstrike-rustapi")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve {
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,
        #[arg(long, env = "RUSTAPI_LISTEN", default_value = "127.0.0.1:51283")]
        listen: SocketAddr,
        #[arg(long, env = "AGENT_RUNTIME_BINARY_PATH")]
        runtime_binary_path: Option<String>,
        #[arg(long, env = "AGENT_RUNTIME_WORK_DIR")]
        runtime_work_dir: Option<String>,
        #[arg(long, env = "AGENT_RUNTIME_MAX_STEPS", default_value_t = 100)]
        runtime_max_steps: i64,
        #[arg(
            long,
            env = "AGENT_RUNTIME_TOOL_TIMEOUT_SECONDS",
            default_value_t = 300
        )]
        runtime_tool_timeout_seconds: i64,
        #[arg(long, env = "AGENT_RUNTIME_MCP_ENDPOINT_URL")]
        runtime_mcp_endpoint_url: Option<String>,
        #[arg(long, env = "AGENT_RUNTIME_MCP_AUTH_HEADER")]
        runtime_mcp_auth_header: Option<String>,
        #[arg(long, env = "AGENT_RUNTIME_MCP_AUTH_HEADER_VALUE")]
        runtime_mcp_auth_header_value: Option<String>,
        #[arg(long, env = "AGENT_RUNTIME_SKILLS_DIR")]
        runtime_skills_dir: Option<String>,
    },
}

#[derive(Clone)]
struct AppState {
    db: PgPool,
    http: reqwest::Client,
    runtime_processes: RuntimeProcessRegistry,
    task_events: AgentRuntimeTaskEventBus,
    permissions: PermissionManager,
    internal_base_url: String,
    runtime: RuntimeSettings,
}

#[derive(Debug, Clone)]
struct RuntimeSettings {
    binary_path: String,
    work_dir: String,
    max_steps: i64,
    tool_timeout_seconds: i64,
    mcp_endpoint_url: String,
    mcp_auth_header: String,
    mcp_auth_header_value: String,
    skills_dir: String,
}

type RuntimeProcessRegistry = Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>;
type AgentRuntimeTaskEventBus = broadcast::Sender<StoredAgentRuntimeTaskEvent>;
type PermissionManager = Arc<Mutex<PermissionState>>;

#[derive(Debug, Default)]
struct PermissionState {
    pending: HashMap<String, PendingPermissionWaiter>,
    session_rules: HashMap<String, Vec<PermissionRule>>,
}

#[derive(Debug)]
struct PendingPermissionWaiter {
    request: PermissionRequest,
    tx: oneshot::Sender<PermissionReplyPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum PermissionAction {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum PermissionReply {
    Once,
    Always,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PermissionRule {
    permission: String,
    pattern: String,
    action: PermissionAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PermissionRequest {
    id: String,
    #[serde(rename = "conversationId")]
    conversation_id: String,
    #[serde(rename = "sessionId", default)]
    session_id: String,
    #[serde(rename = "messageId", default)]
    message_id: String,
    #[serde(rename = "toolName")]
    tool_name: String,
    #[serde(rename = "toolCallId")]
    tool_call_id: String,
    permission: String,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    always: bool,
    #[serde(default)]
    metadata: Value,
    #[serde(default)]
    payload: Value,
    #[serde(default, alias = "timeoutSeconds")]
    timeout_seconds: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PermissionReplyPayload {
    reply: PermissionReply,
    decision: String,
    comment: String,
    #[serde(rename = "editedArguments", skip_serializing_if = "Option::is_none")]
    edited_arguments: Option<Value>,
    resumed: bool,
}

#[derive(Debug, Serialize)]
struct PermissionAskResponse {
    ok: bool,
    action: PermissionAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply: Option<PermissionReply>,
    #[serde(default)]
    resumed: bool,
    #[serde(default)]
    comment: String,
    #[serde(rename = "editedArguments", skip_serializing_if = "Option::is_none")]
    edited_arguments: Option<Value>,
}

#[derive(Debug, Serialize)]
struct HitlDecisionResponse {
    ok: bool,
    resumed: bool,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn internal(err: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: err.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

impl From<sqlx_core::error::Error> for ApiError {
    fn from(err: sqlx_core::error::Error) -> Self {
        Self::internal(err)
    }
}

#[derive(Debug, Deserialize)]
struct ListModelsRequest {
    provider: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
}

#[derive(Debug, Serialize)]
struct ListModelsResponse {
    success: bool,
    supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    models: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIModelsResponse {
    data: Vec<OpenAIModelItem>,
}

#[derive(Debug, Deserialize)]
struct OpenAIModelItem {
    id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RoleItem {
    name: String,
    description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    prompt: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    user_prompt: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    icon: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mcps: Vec<String>,
    enabled: bool,
}

#[derive(Debug, Serialize)]
struct RolesResponse {
    roles: Vec<RoleItem>,
}

#[derive(Debug, Deserialize)]
struct UpsertRoleRequest {
    name: String,
    value: Value,
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ListProjectsQuery {
    status: Option<String>,
    search: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Serialize)]
struct ProjectItem {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    scope_json: String,
    status: String,
    pinned: bool,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct ProjectsResponse {
    projects: Vec<ProjectItem>,
    total: i64,
    limit: i64,
    offset: i64,
}

#[derive(Debug, Deserialize)]
struct UpsertProjectRequest {
    id: String,
    name: String,
    description: Option<String>,
    #[serde(rename = "scope_json")]
    scope_json: Option<String>,
    status: Option<String>,
    pinned: Option<bool>,
    #[serde(rename = "created_at")]
    created_at: Option<String>,
    #[serde(rename = "updated_at")]
    updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListConversationsQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    search: Option<String>,
    #[serde(rename = "sort_by")]
    sort_by: Option<String>,
}

#[derive(Debug, Serialize)]
struct ConversationItem {
    id: String,
    title: String,
    #[serde(rename = "projectId", skip_serializing_if = "String::is_empty")]
    project_id: String,
    pinned: bool,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    messages: Vec<MessageItem>,
}

#[derive(Debug, Serialize)]
struct ListConversationsResponse {
    conversations: Vec<ConversationItem>,
    total: i64,
    limit: i64,
    offset: i64,
}

#[derive(Debug, Serialize)]
struct MessageItem {
    id: String,
    #[serde(rename = "conversationId")]
    conversation_id: String,
    role: String,
    content: String,
    #[serde(rename = "reasoningContent", skip_serializing_if = "String::is_empty")]
    reasoning_content: String,
    #[serde(rename = "mcpExecutionIds", skip_serializing_if = "Vec::is_empty")]
    mcp_execution_ids: Vec<String>,
    #[serde(rename = "processDetails", skip_serializing_if = "Vec::is_empty")]
    process_details: Vec<ProcessDetailItem>,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct ProcessDetailItem {
    id: String,
    #[serde(rename = "messageId")]
    message_id: String,
    #[serde(rename = "conversationId")]
    conversation_id: String,
    #[serde(rename = "eventType")]
    event_type: String,
    message: String,
    data: Value,
    #[serde(rename = "createdAt")]
    created_at: String,
}

#[derive(Debug, Serialize)]
struct ProcessDetailsResponse {
    #[serde(rename = "processDetails")]
    process_details: Vec<ProcessDetailItem>,
}

#[derive(Debug, Deserialize)]
struct CreateConversationRequest {
    title: Option<String>,
    #[serde(rename = "projectId")]
    project_id: Option<String>,
    id: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
    pinned: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct UpdateConversationRequest {
    title: String,
}

#[derive(Debug, Deserialize)]
struct SetConversationProjectRequest {
    #[serde(rename = "projectId")]
    project_id: String,
}

#[derive(Debug, Deserialize)]
struct UpsertConversationRequest {
    id: String,
    title: String,
    #[serde(rename = "projectId")]
    project_id: Option<String>,
    pinned: Option<bool>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpsertMessageRequest {
    id: String,
    #[serde(rename = "conversationId")]
    conversation_id: String,
    role: String,
    content: String,
    #[serde(rename = "reasoningContent")]
    reasoning_content: Option<String>,
    #[serde(rename = "mcpExecutionIds")]
    mcp_execution_ids: Option<Vec<String>>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpsertProcessDetailRequest {
    id: String,
    #[serde(rename = "messageId")]
    message_id: String,
    #[serde(rename = "conversationId")]
    conversation_id: String,
    #[serde(rename = "eventType")]
    event_type: String,
    message: Option<String>,
    data: Option<Value>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct DeleteConversationResponse {
    message: String,
}

#[derive(Debug, Serialize)]
struct SetConversationProjectResponse {
    success: bool,
    #[serde(rename = "projectId")]
    project_id: String,
}

#[derive(Debug, Serialize)]
struct AgentRuntimeTaskItem {
    #[serde(rename = "conversationId")]
    conversation_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    message: String,
    #[serde(rename = "startedAt")]
    started_at: String,
    status: String,
    #[serde(rename = "agentMode", skip_serializing_if = "String::is_empty")]
    agent_mode: String,
    #[serde(
        rename = "assistantMessageId",
        skip_serializing_if = "String::is_empty"
    )]
    assistant_message_id: String,
}

#[derive(Debug, Serialize)]
struct AgentRuntimeTasksResponse {
    tasks: Vec<AgentRuntimeTaskItem>,
}

#[derive(Debug, Serialize)]
struct AgentRuntimeFinalResponse {
    response: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RuntimeTodoItem {
    #[serde(rename = "itemId")]
    item_id: String,
    content: String,
    status: String,
    position: i64,
    #[serde(rename = "updatedAt", skip_serializing_if = "String::is_empty")]
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct RuntimeTodosResponse {
    #[serde(rename = "conversationId")]
    conversation_id: String,
    todos: Vec<RuntimeTodoItem>,
}

#[derive(Debug, Deserialize)]
struct AcceptAgentRuntimeStreamRequest {
    #[serde(rename = "conversationId", default)]
    conversation_id: String,
    message: Option<String>,
    #[serde(rename = "projectId")]
    project_id: Option<String>,
    role: Option<String>,
    reasoning: Option<Value>,
    hitl: Option<HitlConfig>,
    attachments: Option<Vec<ChatAttachment>>,
    #[serde(rename = "webshellConnectionId")]
    webshell_connection_id: Option<String>,
    #[serde(rename = "agentMode")]
    agent_mode: Option<String>,
    background: Option<bool>,
    #[serde(rename = "assistantMessageId")]
    assistant_message_id: Option<String>,
    #[serde(rename = "userMessageId")]
    user_message_id: Option<String>,
    #[serde(rename = "startedAt")]
    started_at: Option<String>,
    #[serde(rename = "createdNew")]
    created_new: Option<bool>,
    #[serde(rename = "runtimeBinaryPath")]
    runtime_binary_path: Option<String>,
    #[serde(rename = "runtimeWorkDir")]
    runtime_work_dir: Option<String>,
    #[serde(rename = "runtimeCommand")]
    runtime_command: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct ChatAttachment {
    #[serde(rename = "fileName", default)]
    file_name: String,
    #[serde(default)]
    content: String,
    #[serde(rename = "mimeType", default)]
    mime_type: String,
    #[serde(rename = "serverPath", default)]
    server_path: String,
}

#[derive(Debug, Deserialize)]
struct AgentRuntimeTaskEventsQuery {
    #[serde(rename = "conversationId")]
    conversation_id: Option<String>,
    #[serde(rename = "afterEventId")]
    after_event_id: Option<String>,
    #[serde(rename = "runtimeEventId")]
    runtime_event_id: Option<String>,
    limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct CreateAgentRuntimeTaskEventRequest {
    #[serde(rename = "conversationId")]
    conversation_id: String,
    line: String,
    #[serde(rename = "runtimeEventId")]
    runtime_event_id: Option<String>,
    #[serde(rename = "eventType")]
    event_type: Option<String>,
    terminal: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedAgentRuntimeTaskEventsQuery {
    conversation_id: String,
    after_id: i64,
    after_runtime_event_id: String,
    limit: i64,
    scoped_to_conversation: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct NormalizedAgentRuntimeTaskEventInput {
    conversation_id: String,
    line: String,
    runtime_event_id: String,
    event_type: String,
    terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredAgentRuntimeTaskEvent {
    id: i64,
    conversation_id: String,
    event_type: String,
    line: String,
    terminal: bool,
}

#[derive(Debug, Deserialize)]
struct UpsertAgentRuntimeTaskRequest {
    message: Option<String>,
    status: Option<String>,
    #[serde(rename = "agentMode")]
    agent_mode: Option<String>,
    #[serde(rename = "assistantMessageId")]
    assistant_message_id: Option<String>,
    #[serde(rename = "startedAt")]
    started_at: Option<String>,
    active: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct CancelAgentRuntimeTaskRequest {
    #[serde(rename = "conversationId")]
    conversation_id: String,
    reason: Option<String>,
    #[serde(rename = "continueAfter")]
    continue_after: Option<bool>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct CancelAgentRuntimeTaskResponse {
    status: String,
    #[serde(rename = "conversationId")]
    conversation_id: String,
    message: String,
    #[serde(rename = "continueAfter")]
    continue_after: bool,
    #[serde(rename = "interruptWithNote")]
    interrupt_with_note: bool,
    #[serde(rename = "agentMode")]
    agent_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct HitlConfig {
    enabled: bool,
    mode: String,
    #[serde(rename = "sensitiveTools")]
    sensitive_tools: Vec<String>,
    #[serde(rename = "timeoutSeconds")]
    timeout_seconds: i64,
}

#[derive(Debug, Serialize)]
struct HitlConfigResponse {
    #[serde(rename = "conversationId")]
    conversation_id: String,
    hitl: HitlConfig,
    #[serde(rename = "hitlGlobalToolWhitelist")]
    hitl_global_tool_whitelist: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct UpsertHitlConfigRequest {
    #[serde(rename = "conversationId")]
    conversation_id: String,
    enabled: bool,
    mode: Option<String>,
    #[serde(rename = "sensitiveTools")]
    sensitive_tools: Option<Vec<String>>,
    #[serde(rename = "timeoutSeconds")]
    timeout_seconds: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct UpsertHitlInterruptRequest {
    id: String,
    #[serde(rename = "conversationId")]
    conversation_id: String,
    #[serde(rename = "messageId")]
    message_id: Option<String>,
    mode: String,
    #[serde(rename = "toolName")]
    tool_name: String,
    #[serde(rename = "toolCallId")]
    tool_call_id: Option<String>,
    payload: Option<String>,
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListHitlPendingQuery {
    #[serde(rename = "conversationId")]
    conversation_id: Option<String>,
    status: Option<String>,
    page: Option<i64>,
    #[serde(rename = "pageSize")]
    page_size: Option<i64>,
}

#[derive(Debug, Serialize)]
struct HitlPendingItem {
    id: String,
    #[serde(rename = "conversationId")]
    conversation_id: String,
    #[serde(rename = "messageId")]
    message_id: String,
    mode: String,
    #[serde(rename = "toolName")]
    tool_name: String,
    #[serde(rename = "toolCallId")]
    tool_call_id: String,
    payload: String,
    permission: String,
    patterns: Vec<String>,
    always: bool,
    metadata: Value,
    status: String,
    decision: String,
    comment: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "decidedAt")]
    decided_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct HitlPendingResponse {
    items: Vec<HitlPendingItem>,
    page: i64,
    #[serde(rename = "pageSize")]
    page_size: i64,
}

#[derive(Debug, Clone)]
struct HitlEventFields {
    interrupt_id: String,
    tool_name: String,
    tool_call_id: String,
    status: String,
    permission: String,
    patterns: Vec<String>,
    always: bool,
    metadata: Value,
}

#[derive(Debug, Deserialize)]
struct HitlDecisionRequest {
    #[serde(rename = "interruptId")]
    interrupt_id: String,
    #[serde(default)]
    decision: String,
    #[serde(default)]
    reply: Option<String>,
    comment: Option<String>,
    #[serde(rename = "editedArguments")]
    edited_arguments: Option<Value>,
}

#[derive(Debug, Serialize)]
struct OkResponse {
    ok: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Serve {
            database_url,
            listen,
            runtime_binary_path,
            runtime_work_dir,
            runtime_max_steps,
            runtime_tool_timeout_seconds,
            runtime_mcp_endpoint_url,
            runtime_mcp_auth_header,
            runtime_mcp_auth_header_value,
            runtime_skills_dir,
        } => {
            let runtime = RuntimeSettings {
                binary_path: runtime_binary_path.unwrap_or_default().trim().to_string(),
                work_dir: runtime_work_dir.unwrap_or_default().trim().to_string(),
                max_steps: runtime_max_steps.max(1),
                tool_timeout_seconds: runtime_tool_timeout_seconds.max(1),
                mcp_endpoint_url: runtime_mcp_endpoint_url
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                mcp_auth_header: runtime_mcp_auth_header
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                mcp_auth_header_value: runtime_mcp_auth_header_value
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                skills_dir: runtime_skills_dir.unwrap_or_default().trim().to_string(),
            };
            serve(database_url, listen, runtime).await
        }
    }
}

async fn serve(
    database_url: String,
    listen: SocketAddr,
    runtime: RuntimeSettings,
) -> anyhow::Result<()> {
    let db = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;
    ensure_schema(&db).await?;

    let state = Arc::new(AppState {
        db,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?,
        runtime_processes: Arc::new(Mutex::new(HashMap::new())),
        task_events: broadcast::channel(4096).0,
        permissions: Arc::new(Mutex::new(PermissionState::default())),
        internal_base_url: internal_base_url(&listen),
        runtime,
    });
    let app = Router::new()
        .route("/api/config", get(get_config).put(update_config))
        .route("/api/config/list-models", post(list_models))
        .route("/api/roles", get(list_roles))
        .route("/api/internal/roles", post(upsert_role))
        .route("/api/internal/roles/{name}", delete(delete_role))
        .route("/api/projects", get(list_projects))
        .route("/api/internal/projects", post(upsert_project))
        .route("/api/internal/projects/{id}", delete(delete_project))
        .route(
            "/api/conversations",
            get(list_conversations).post(create_conversation),
        )
        .route(
            "/api/conversations/{id}",
            get(get_conversation)
                .put(update_conversation)
                .delete(delete_conversation),
        )
        .route(
            "/api/conversations/{id}/project",
            put(set_conversation_project),
        )
        .route(
            "/api/messages/{id}/process-details",
            get(get_message_process_details),
        )
        .route("/api/internal/conversations", post(upsert_conversation))
        .route("/api/internal/messages", post(upsert_message))
        .route("/api/internal/process-details", post(upsert_process_detail))
        .route(
            "/api/agent-runtime/stream",
            post(accept_agent_runtime_stream),
        )
        .route("/api/agent-loop/tasks", get(list_agent_runtime_tasks))
        .route(
            "/api/agent-loop/task-events",
            get(stream_agent_runtime_task_events),
        )
        .route(
            "/api/conversations/{id}/runtime-todos",
            get(get_runtime_todos),
        )
        .route("/api/agent-loop/cancel", post(cancel_agent_runtime_task))
        .route(
            "/api/internal/agent-runtime/tasks/{conversation_id}",
            get(get_agent_runtime_task).put(upsert_agent_runtime_task),
        )
        .route(
            "/api/internal/agent-runtime/final-response/{conversation_id}",
            get(get_agent_runtime_final_response),
        )
        .route(
            "/api/internal/agent-runtime/task-events",
            post(create_agent_runtime_task_event),
        )
        .route("/api/hitl/config/{conversation_id}", get(get_hitl_config))
        .route("/api/hitl/config", put(upsert_hitl_config))
        .route("/api/internal/hitl/interrupts", post(upsert_hitl_interrupt))
        .route("/api/internal/hitl/permission-ask", post(permission_ask))
        .route("/api/hitl/pending", get(list_hitl_pending))
        .route("/api/hitl/decision", post(decide_hitl_interrupt))
        .with_state(state);

    let listener = TcpListener::bind(listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn get_config(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let value = load_or_initialize_config(&state.db).await?;
    Ok(Json(value))
}

async fn list_roles(State(state): State<Arc<AppState>>) -> Result<Json<RolesResponse>, ApiError> {
    let roles = query_roles(&state.db).await?;
    Ok(Json(RolesResponse { roles }))
}

async fn upsert_role(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpsertRoleRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }
    save_role(&state.db, &name, &req.value, req.enabled.unwrap_or(true)).await?;
    Ok(Json(OkResponse { ok: true }))
}

async fn delete_role(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<OkResponse>, ApiError> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }
    query("DELETE FROM roles WHERE name = $1")
        .bind(&name)
        .execute(&state.db)
        .await?;
    Ok(Json(OkResponse { ok: true }))
}

async fn list_projects(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListProjectsQuery>,
) -> Result<Json<ProjectsResponse>, ApiError> {
    let normalized = query.normalized();
    let projects = query_projects(&state.db, &normalized).await?;
    let total = count_projects(&state.db, &normalized).await?;
    Ok(Json(ProjectsResponse {
        projects,
        total,
        limit: normalized.limit,
        offset: normalized.offset,
    }))
}

async fn upsert_project(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpsertProjectRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    let project = normalize_project_input(req)?;
    save_project(&state.db, &project).await?;
    Ok(Json(OkResponse { ok: true }))
}

async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<OkResponse>, ApiError> {
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::bad_request("id is required"));
    }
    query("DELETE FROM projects WHERE id = $1")
        .bind(&id)
        .execute(&state.db)
        .await?;
    Ok(Json(OkResponse { ok: true }))
}

async fn list_conversations(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListConversationsQuery>,
) -> Result<Json<ListConversationsResponse>, ApiError> {
    let normalized = query.normalized();
    let conversations = query_conversations(&state.db, &normalized).await?;
    let total = count_conversations(&state.db, &normalized).await?;
    Ok(Json(ListConversationsResponse {
        conversations,
        total,
        limit: normalized.limit,
        offset: normalized.offset,
    }))
}

async fn create_conversation(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateConversationRequest>,
) -> Result<Json<ConversationItem>, ApiError> {
    let title = normalize_optional_text(req.title).unwrap_or_else(|| "新对话".to_string());
    let id = normalize_optional_text(req.id).unwrap_or_else(|| Uuid::new_v4().to_string());
    let project_id = normalize_optional_text(req.project_id).unwrap_or_default();
    if !project_id.is_empty() {
        ensure_project_exists(&state.db, &project_id).await?;
    }
    let created_at = req.created_at.unwrap_or_default().trim().to_string();
    let updated_at = req.updated_at.unwrap_or_default().trim().to_string();
    let pinned = req.pinned.unwrap_or(false);
    save_conversation(
        &state.db,
        &NormalizedConversationInput {
            id: id.clone(),
            title,
            project_id,
            pinned,
            created_at,
            updated_at,
        },
    )
    .await?;
    let conv = query_conversation(&state.db, &id, false).await?;
    Ok(Json(conv))
}

async fn get_conversation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<ConversationItem>, ApiError> {
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::bad_request("conversation id required"));
    }
    let include_process_details = query
        .get("include_process_details")
        .map(|value| truthy(value))
        .unwrap_or(false);
    let conv = query_conversation(&state.db, &id, include_process_details).await?;
    Ok(Json(conv))
}

async fn update_conversation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateConversationRequest>,
) -> Result<Json<ConversationItem>, ApiError> {
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::bad_request("conversation id required"));
    }
    let title = req.title.trim().to_string();
    if title.is_empty() {
        return Err(ApiError::bad_request("标题不能为空"));
    }
    let rows = query("UPDATE conversations SET title = $1 WHERE id = $2")
        .bind(&title)
        .bind(&id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "对话不存在".to_string(),
        });
    }
    let conv = query_conversation(&state.db, &id, false).await?;
    Ok(Json(conv))
}

async fn set_conversation_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<SetConversationProjectRequest>,
) -> Result<Json<SetConversationProjectResponse>, ApiError> {
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::bad_request("conversation id required"));
    }
    if !conversation_exists(&state.db, &id).await? {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "对话不存在".to_string(),
        });
    }
    let project_id = req.project_id.trim().to_string();
    if !project_id.is_empty() {
        ensure_project_exists(&state.db, &project_id).await?;
    }
    query(
        r#"
        UPDATE conversations
        SET project_id = NULLIF($1, ''), updated_at = NOW()
        WHERE id = $2
        "#,
    )
    .bind(&project_id)
    .bind(&id)
    .execute(&state.db)
    .await?;
    Ok(Json(SetConversationProjectResponse {
        success: true,
        project_id,
    }))
}

async fn delete_conversation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<DeleteConversationResponse>, ApiError> {
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::bad_request("conversation id required"));
    }
    query("DELETE FROM conversations WHERE id = $1")
        .bind(&id)
        .execute(&state.db)
        .await?;
    Ok(Json(DeleteConversationResponse {
        message: "删除成功".to_string(),
    }))
}

async fn get_message_process_details(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ProcessDetailsResponse>, ApiError> {
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::bad_request("message id required"));
    }
    let process_details = query_process_details(&state.db, &id).await?;
    Ok(Json(ProcessDetailsResponse { process_details }))
}

async fn upsert_conversation(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpsertConversationRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    let conv = normalize_conversation_input(req)?;
    save_conversation(&state.db, &conv).await?;
    Ok(Json(OkResponse { ok: true }))
}

async fn upsert_message(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpsertMessageRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    let msg = normalize_message_input(req)?;
    save_message(&state.db, &msg).await?;
    Ok(Json(OkResponse { ok: true }))
}

async fn upsert_process_detail(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpsertProcessDetailRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    let detail = normalize_process_detail_input(req)?;
    save_process_detail(&state.db, &detail).await?;
    Ok(Json(OkResponse { ok: true }))
}

async fn list_agent_runtime_tasks(
    State(state): State<Arc<AppState>>,
) -> Result<Json<AgentRuntimeTasksResponse>, ApiError> {
    let tasks = query_agent_runtime_tasks(&state.db).await?;
    Ok(Json(AgentRuntimeTasksResponse { tasks }))
}

async fn accept_agent_runtime_stream(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AcceptAgentRuntimeStreamRequest>,
) -> Result<Response, ApiError> {
    let run = if request_has_legacy_runtime_fields(&req) {
        normalize_agent_runtime_stream_input(req)?
    } else {
        let turn = prepare_agent_runtime_frontend_turn(&state, req).await?;
        frontend_turn_to_stream_run(&turn)
    };
    ensure_no_active_agent_runtime_task(&state.db, &run.conversation_id).await?;
    save_agent_runtime_stream_run(&state.db, &run).await?;
    save_and_publish_agent_runtime_task_state(
        &state.db,
        &state.task_events,
        &NormalizedAgentRuntimeTaskInput {
            conversation_id: run.conversation_id.clone(),
            message: run.message.clone(),
            status: "running".to_string(),
            agent_mode: run.agent_mode.clone(),
            assistant_message_id: run.assistant_message_id.clone(),
            started_at: run.started_at.clone(),
            active: true,
        },
    )
    .await?;
    spawn_agent_runtime_jsonl_run(
        state.internal_base_url.clone(),
        state.db.clone(),
        state.runtime_processes.clone(),
        state.task_events.clone(),
        run.clone(),
    );
    Ok(agent_runtime_stream_accepted_sse_response(&run))
}

async fn stream_agent_runtime_task_events(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<AgentRuntimeTaskEventsQuery>,
) -> Result<Response, ApiError> {
    let q = query.normalized(&headers);
    Ok(agent_runtime_task_events_sse_response(
        state.db.clone(),
        state.task_events.clone(),
        q,
    ))
}

async fn create_agent_runtime_task_event(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateAgentRuntimeTaskEventRequest>,
) -> Result<Json<Value>, ApiError> {
    let event = normalize_agent_runtime_task_event_input(req)?;
    let id =
        save_and_publish_agent_runtime_task_event(&state.db, &state.task_events, &event).await?;
    Ok(Json(json!({"ok": true, "id": id})))
}

async fn get_runtime_todos(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
) -> Result<Json<RuntimeTodosResponse>, ApiError> {
    let conversation_id = conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Err(ApiError::bad_request("conversationId is required"));
    }
    let todos = query_runtime_todos(&state.db, &conversation_id).await?;
    Ok(Json(RuntimeTodosResponse {
        conversation_id,
        todos,
    }))
}

async fn upsert_agent_runtime_task(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
    Json(req): Json<UpsertAgentRuntimeTaskRequest>,
) -> Result<Json<Value>, ApiError> {
    let task = normalize_agent_runtime_task_input(conversation_id, req)?;
    save_and_publish_agent_runtime_task_state(&state.db, &state.task_events, &task).await?;
    Ok(Json(json!({"ok": true})))
}

async fn get_agent_runtime_task(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let conversation_id = conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Err(ApiError::bad_request("conversationId is required"));
    }
    let task = query_agent_runtime_task(&state.db, &conversation_id).await?;
    Ok(Json(json!({"task": task})))
}

async fn get_agent_runtime_final_response(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
) -> Result<Json<AgentRuntimeFinalResponse>, ApiError> {
    let conversation_id = conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Err(ApiError::bad_request("conversationId is required"));
    }
    let response = query_agent_runtime_final_response(&state.db, &conversation_id).await?;
    Ok(Json(AgentRuntimeFinalResponse { response }))
}

async fn cancel_agent_runtime_task(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CancelAgentRuntimeTaskRequest>,
) -> Result<Json<CancelAgentRuntimeTaskResponse>, ApiError> {
    let conversation_id = req.conversation_id.trim().to_string();
    if !conversation_id.is_empty() {
        if let Some(cancel) = state
            .runtime_processes
            .lock()
            .await
            .remove(&conversation_id)
        {
            let _ = cancel.send(());
        }
    }
    let response = cancel_agent_runtime_task_in_db(&state.db, &state.task_events, req).await?;
    Ok(Json(response))
}

async fn get_hitl_config(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
) -> Result<Json<HitlConfigResponse>, ApiError> {
    let conversation_id = conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Err(ApiError::bad_request("conversationId is required"));
    }
    let mut hitl = load_hitl_config(&state.db, &conversation_id).await?;
    if !hitl_config_effective(&hitl) {
        if let Some(mode) = latest_pending_hitl_mode(&state.db, &conversation_id).await? {
            hitl.enabled = true;
            hitl.mode = normalize_hitl_mode(&mode);
            hitl.timeout_seconds = hitl.timeout_seconds.max(0);
        }
    }
    Ok(Json(HitlConfigResponse {
        conversation_id,
        hitl,
        hitl_global_tool_whitelist: Vec::new(),
    }))
}

async fn upsert_hitl_config(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpsertHitlConfigRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    let conversation_id = req.conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Err(ApiError::bad_request("conversationId is required"));
    }
    let hitl = normalize_hitl_config(HitlConfig {
        enabled: req.enabled,
        mode: req.mode.unwrap_or_default(),
        sensitive_tools: clean_string_list(req.sensitive_tools.unwrap_or_default()),
        timeout_seconds: req.timeout_seconds.unwrap_or(0),
    });
    save_hitl_config(&state.db, &conversation_id, &hitl).await?;
    Ok(Json(OkResponse { ok: true }))
}

async fn list_hitl_pending(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListHitlPendingQuery>,
) -> Result<Json<HitlPendingResponse>, ApiError> {
    let normalized = query.normalized();
    let items = query_hitl_pending(&state.db, &normalized).await?;
    Ok(Json(HitlPendingResponse {
        items,
        page: normalized.page,
        page_size: normalized.page_size,
    }))
}

async fn upsert_hitl_interrupt(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpsertHitlInterruptRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    let interrupt = normalize_hitl_interrupt_input(req)?;
    save_hitl_interrupt(&state.db, &interrupt).await?;
    publish_hitl_pending_snapshot(
        &state.db,
        &state.task_events,
        &interrupt.conversation_id,
        "hitl_pending_updated",
        Some(&interrupt.id),
        None,
        None,
    )
    .await?;
    Ok(Json(OkResponse { ok: true }))
}

async fn permission_ask(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PermissionRequest>,
) -> Result<Json<PermissionAskResponse>, ApiError> {
    let request = normalize_permission_request(req)?;
    let rule_action = {
        let permissions = state.permissions.lock().await;
        permission_rules_decide(
            &permissions,
            &permission_session_key(&request.conversation_id, &request.session_id),
            &request.permission,
            &request.patterns,
            &request.tool_name,
        )
    };
    match rule_action {
        Some(PermissionAction::Allow) => {
            return Ok(Json(PermissionAskResponse {
                ok: true,
                action: PermissionAction::Allow,
                reply: Some(PermissionReply::Once),
                resumed: true,
                comment: String::new(),
                edited_arguments: None,
            }));
        }
        Some(PermissionAction::Deny) => {
            return Ok(Json(PermissionAskResponse {
                ok: true,
                action: PermissionAction::Deny,
                reply: Some(PermissionReply::Reject),
                resumed: true,
                comment: "permission denied by session rule".to_string(),
                edited_arguments: None,
            }));
        }
        Some(PermissionAction::Ask) | None => {}
    }

    let (tx, rx) = oneshot::channel();
    state.permissions.lock().await.pending.insert(
        request.id.clone(),
        PendingPermissionWaiter {
            request: request.clone(),
            tx,
        },
    );

    let payload = permission_request_payload(&request);
    if let Err(err) = save_hitl_interrupt(
        &state.db,
        &NormalizedHitlInterruptInput {
            id: request.id.clone(),
            conversation_id: request.conversation_id.clone(),
            message_id: request.message_id.clone(),
            mode: "approval".to_string(),
            tool_name: request.tool_name.clone(),
            tool_call_id: request.tool_call_id.clone(),
            payload: payload.to_string(),
            status: "pending".to_string(),
        },
    )
    .await
    {
        state.permissions.lock().await.pending.remove(&request.id);
        return Err(err.into());
    }

    if let Err(err) = publish_hitl_pending_snapshot(
        &state.db,
        &state.task_events,
        &request.conversation_id,
        "hitl_pending_updated",
        Some(&request.id),
        None,
        None,
    )
    .await
    {
        state.permissions.lock().await.pending.remove(&request.id);
        return Err(err.into());
    }

    let timeout_seconds = if request.timeout_seconds <= 0 {
        600
    } else {
        request.timeout_seconds
    };
    let reply = match time::timeout(Duration::from_secs(timeout_seconds as u64), rx).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(_closed)) => {
            let comment = "permission waiter closed".to_string();
            query(
                r#"
                UPDATE hitl_interrupts
                SET status = 'cancelled', decision = 'reject', decision_comment = $1, decided_at = NOW()
                WHERE id = $2 AND status = 'pending'
                "#,
            )
            .bind(&comment)
            .bind(&request.id)
            .execute(&state.db)
            .await?;
            publish_hitl_pending_snapshot(
                &state.db,
                &state.task_events,
                &request.conversation_id,
                "hitl_decision_updated",
                Some(&request.id),
                Some("reject"),
                Some(&comment),
            )
            .await?;
            PermissionReplyPayload {
                reply: PermissionReply::Reject,
                decision: "reject".to_string(),
                comment,
                edited_arguments: None,
                resumed: false,
            }
        }
        Err(_elapsed) => {
            let comment = "permission request timed out".to_string();
            state.permissions.lock().await.pending.remove(&request.id);
            query(
                r#"
                UPDATE hitl_interrupts
                SET status = 'timeout', decision = 'reject', decision_comment = $1, decided_at = NOW()
                WHERE id = $2 AND status = 'pending'
                "#,
            )
            .bind(&comment)
            .bind(&request.id)
            .execute(&state.db)
            .await?;
            publish_hitl_pending_snapshot(
                &state.db,
                &state.task_events,
                &request.conversation_id,
                "hitl_decision_updated",
                Some(&request.id),
                Some("reject"),
                Some(&comment),
            )
            .await?;
            PermissionReplyPayload {
                reply: PermissionReply::Reject,
                decision: "reject".to_string(),
                comment,
                edited_arguments: None,
                resumed: false,
            }
        }
    };

    Ok(Json(PermissionAskResponse {
        ok: true,
        action: if reply.reply == PermissionReply::Reject {
            PermissionAction::Deny
        } else {
            PermissionAction::Allow
        },
        reply: Some(reply.reply),
        resumed: reply.resumed,
        comment: reply.comment,
        edited_arguments: reply.edited_arguments,
    }))
}

async fn decide_hitl_interrupt(
    State(state): State<Arc<AppState>>,
    Json(req): Json<HitlDecisionRequest>,
) -> Result<Json<HitlDecisionResponse>, ApiError> {
    let interrupt_id = req.interrupt_id.trim().to_string();
    if interrupt_id.is_empty() {
        return Err(ApiError::bad_request("interruptId is required"));
    }
    let reply = normalize_permission_reply(req.reply.as_deref(), &req.decision)?;
    let decision = permission_reply_decision(&reply).to_string();
    let comment = req.comment.unwrap_or_default().trim().to_string();
    let edited_arguments = req.edited_arguments;
    let result = query(
        r#"
        UPDATE hitl_interrupts
        SET status = 'decided', decision = $1, decision_comment = $2, edited_arguments = $3, decided_at = NOW()
        WHERE id = $4 AND status = 'pending'
        "#,
    )
    .bind(&decision)
    .bind(&comment)
    .bind(&edited_arguments)
    .bind(&interrupt_id)
    .execute(&state.db)
    .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: "interrupt not found or already resolved".to_string(),
        });
    }
    if let Some(conversation_id) =
        query_hitl_interrupt_conversation_id(&state.db, &interrupt_id).await?
    {
        publish_hitl_pending_snapshot(
            &state.db,
            &state.task_events,
            &conversation_id,
            "hitl_decision_updated",
            Some(&interrupt_id),
            Some(&decision),
            Some(&comment),
        )
        .await?;
    }

    let mut resumed = false;
    let mut permissions = state.permissions.lock().await;
    if let Some(waiter) = permissions.pending.remove(&interrupt_id) {
        if reply == PermissionReply::Always {
            let key =
                permission_session_key(&waiter.request.conversation_id, &waiter.request.session_id);
            let rules = permissions.session_rules.entry(key).or_default();
            for pattern in permission_request_patterns(&waiter.request) {
                rules.push(PermissionRule {
                    permission: waiter.request.permission.clone(),
                    pattern,
                    action: PermissionAction::Allow,
                });
            }
        }
        resumed = waiter
            .tx
            .send(PermissionReplyPayload {
                reply,
                decision,
                comment,
                edited_arguments,
                resumed: true,
            })
            .is_ok();
    }

    Ok(Json(HitlDecisionResponse { ok: true, resumed }))
}

async fn update_config(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let raw: Value = serde_json::from_slice(&body)
        .map_err(|err| ApiError::bad_request(format!("无效的请求参数: {err}")))?;
    if !raw.is_object() {
        return Err(ApiError::bad_request("config body must be an object"));
    }

    let mut next = load_or_initialize_config(&state.db).await?;
    merge_json_objects(&mut next, frontend_config_patch_projection(raw));
    next = frontend_config_projection(next);
    save_config(&state.db, &next).await?;
    Ok(Json(next))
}

async fn list_models(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<Json<ListModelsResponse>, ApiError> {
    let req = parse_list_models_request(&body)?;
    let current = load_or_initialize_config(&state.db).await?;
    let openai = current.get("openai").and_then(Value::as_object);

    let provider = first_non_empty([
        req.provider.as_deref(),
        openai
            .and_then(|v| v.get("provider"))
            .and_then(Value::as_str),
        Some("openai"),
    ]);
    let api_key = first_non_empty([
        req.api_key.as_deref(),
        openai
            .and_then(|v| v.get("api_key"))
            .and_then(Value::as_str),
    ]);
    let mut base_url = first_non_empty([
        req.base_url.as_deref(),
        openai
            .and_then(|v| v.get("base_url"))
            .and_then(Value::as_str),
    ]);

    if provider.eq_ignore_ascii_case("claude") || provider.eq_ignore_ascii_case("anthropic") {
        return Ok(Json(ListModelsResponse {
            success: false,
            supported: false,
            models: None,
            count: None,
            error: Some(
                "Claude (Anthropic Messages API) 不支持自动获取模型列表，请手动填写".to_string(),
            ),
        }));
    }

    if api_key.trim().is_empty() {
        return Err(ApiError::bad_request("API Key 不能为空"));
    }

    base_url = base_url.trim().trim_end_matches('/').to_string();
    if base_url.is_empty() {
        base_url = "https://api.openai.com/v1".to_string();
    }

    let url = format!("{base_url}/models");
    let resp = match state.http.get(url).bearer_auth(api_key.trim()).send().await {
        Ok(resp) => resp,
        Err(err) => {
            return Ok(Json(ListModelsResponse {
                success: false,
                supported: true,
                models: None,
                count: None,
                error: Some(format!("call openai models api: {err}")),
            }));
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Ok(Json(ListModelsResponse {
            success: false,
            supported: true,
            models: None,
            count: None,
            error: Some(format!("API 返回错误 (HTTP {}): {}", status.as_u16(), text)),
        }));
    }

    let parsed: OpenAIModelsResponse = match resp.json().await {
        Ok(parsed) => parsed,
        Err(err) => {
            return Ok(Json(ListModelsResponse {
                success: false,
                supported: true,
                models: None,
                count: None,
                error: Some(format!("parse models response: {err}")),
            }));
        }
    };
    let mut models: Vec<String> = parsed
        .data
        .into_iter()
        .filter_map(|item| item.id.map(|id| id.trim().to_string()))
        .filter(|id| !id.is_empty())
        .collect();
    models.sort();
    models.dedup();

    Ok(Json(ListModelsResponse {
        success: true,
        supported: true,
        count: Some(models.len()),
        models: Some(models),
        error: None,
    }))
}

async fn ensure_schema(db: &PgPool) -> Result<(), sqlx_core::error::Error> {
    query(
        r#"
        CREATE TABLE IF NOT EXISTS app_config (
            key TEXT PRIMARY KEY,
            value JSONB NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
    )
    .execute(db)
    .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_app_config_updated_at ON app_config(updated_at)")
        .execute(db)
        .await?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS roles (
            name TEXT PRIMARY KEY,
            value JSONB NOT NULL DEFAULT '{}'::jsonb,
            enabled BOOLEAN NOT NULL DEFAULT TRUE,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
    )
    .execute(db)
    .await?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS projects (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            description TEXT,
            scope_json TEXT,
            status TEXT NOT NULL DEFAULT 'active',
            pinned BOOLEAN NOT NULL DEFAULT FALSE,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
    )
    .execute(db)
    .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_projects_status ON projects(status)")
        .execute(db)
        .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_projects_updated_at ON projects(updated_at)")
        .execute(db)
        .await?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS conversations (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            project_id TEXT,
            pinned BOOLEAN NOT NULL DEFAULT FALSE,
            last_react_input TEXT,
            last_react_output TEXT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
    )
    .execute(db)
    .await?;
    query("ALTER TABLE conversations ADD COLUMN IF NOT EXISTS project_id TEXT")
        .execute(db)
        .await?;
    query(
        "ALTER TABLE conversations ADD COLUMN IF NOT EXISTS pinned BOOLEAN NOT NULL DEFAULT FALSE",
    )
    .execute(db)
    .await?;
    query("ALTER TABLE conversations ADD COLUMN IF NOT EXISTS last_react_input TEXT")
        .execute(db)
        .await?;
    query("ALTER TABLE conversations ADD COLUMN IF NOT EXISTS last_react_output TEXT")
        .execute(db)
        .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_conversations_updated_at ON conversations(updated_at DESC)")
        .execute(db)
        .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_conversations_created_at ON conversations(created_at DESC)")
        .execute(db)
        .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_conversations_project_id ON conversations(project_id)")
        .execute(db)
        .await?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS messages (
            id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            reasoning_content TEXT,
            mcp_execution_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
    )
    .execute(db)
    .await?;
    query("ALTER TABLE messages ADD COLUMN IF NOT EXISTS reasoning_content TEXT")
        .execute(db)
        .await?;
    query(
        "ALTER TABLE messages ADD COLUMN IF NOT EXISTS mcp_execution_ids JSONB NOT NULL DEFAULT '[]'::jsonb",
    )
    .execute(db)
    .await?;
    ensure_messages_mcp_execution_ids_jsonb(db).await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_messages_conversation_id ON messages(conversation_id, created_at, id)")
        .execute(db)
        .await?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS process_details (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
            conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
            event_type TEXT NOT NULL,
            message TEXT,
            data JSONB,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
    )
    .execute(db)
    .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_process_details_message_id ON process_details(message_id, created_at, id)")
        .execute(db)
        .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_process_details_conversation_id ON process_details(conversation_id, created_at, id)")
        .execute(db)
        .await?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_runtime_tasks (
            conversation_id TEXT PRIMARY KEY,
            message TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'running',
            agent_mode TEXT NOT NULL DEFAULT 'agent_runtime',
            assistant_message_id TEXT,
            started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            finished_at TIMESTAMPTZ,
            active BOOLEAN NOT NULL DEFAULT TRUE
        )
        "#,
    )
    .execute(db)
    .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_agent_runtime_tasks_active_updated ON agent_runtime_tasks(active, updated_at DESC)")
        .execute(db)
        .await?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_runtime_stream_runs (
            id BIGSERIAL PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            message TEXT NOT NULL DEFAULT '',
            agent_mode TEXT NOT NULL DEFAULT 'agent_runtime',
            assistant_message_id TEXT,
            user_message_id TEXT,
            background BOOLEAN NOT NULL DEFAULT TRUE,
            created_new BOOLEAN NOT NULL DEFAULT FALSE,
            status TEXT NOT NULL DEFAULT 'accepted',
            started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
    )
    .execute(db)
    .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_agent_runtime_stream_runs_conversation_created ON agent_runtime_stream_runs(conversation_id, created_at DESC)")
        .execute(db)
        .await?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_runtime_task_events (
            id BIGSERIAL PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            runtime_event_id TEXT NOT NULL DEFAULT '',
            event_type TEXT NOT NULL DEFAULT '',
            sse_line TEXT NOT NULL,
            terminal BOOLEAN NOT NULL DEFAULT FALSE,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
    )
    .execute(db)
    .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_agent_runtime_task_events_conversation_id ON agent_runtime_task_events(conversation_id, id)")
        .execute(db)
        .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_agent_runtime_task_events_id ON agent_runtime_task_events(id)")
        .execute(db)
        .await?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_runtime_todos (
            conversation_id TEXT NOT NULL,
            item_id TEXT NOT NULL,
            content TEXT NOT NULL,
            status TEXT NOT NULL,
            position BIGINT NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            PRIMARY KEY (conversation_id, item_id)
        )
        "#,
    )
    .execute(db)
    .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_agent_runtime_todos_conversation_position ON agent_runtime_todos(conversation_id, position)")
        .execute(db)
        .await?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS hitl_conversation_configs (
            conversation_id TEXT PRIMARY KEY,
            enabled BOOLEAN NOT NULL DEFAULT FALSE,
            mode TEXT NOT NULL DEFAULT 'off',
            sensitive_tools JSONB NOT NULL DEFAULT '[]'::jsonb,
            timeout_seconds BIGINT NOT NULL DEFAULT 0,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
    )
    .execute(db)
    .await?;
    query(
        "ALTER TABLE hitl_conversation_configs ALTER COLUMN timeout_seconds TYPE BIGINT USING timeout_seconds::bigint",
    )
    .execute(db)
    .await?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS hitl_interrupts (
            id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            message_id TEXT,
            mode TEXT NOT NULL,
            tool_name TEXT NOT NULL,
            tool_call_id TEXT,
            payload TEXT,
            status TEXT NOT NULL,
            decision TEXT,
            decision_comment TEXT,
            edited_arguments JSONB,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            decided_at TIMESTAMPTZ
        )
        "#,
    )
    .execute(db)
    .await?;
    query("ALTER TABLE hitl_interrupts ADD COLUMN IF NOT EXISTS edited_arguments JSONB")
        .execute(db)
        .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_hitl_interrupts_conversation ON hitl_interrupts(conversation_id)")
        .execute(db)
        .await?;
    query("CREATE INDEX IF NOT EXISTS idx_rustapi_hitl_interrupts_status_created ON hitl_interrupts(status, created_at DESC)")
        .execute(db)
        .await?;
    Ok(())
}

async fn query_roles(db: &PgPool) -> Result<Vec<RoleItem>, sqlx_core::error::Error> {
    let rows = query(
        r#"
        SELECT name, value, enabled
        FROM roles
        WHERE enabled = TRUE
        ORDER BY name ASC
        "#,
    )
    .fetch_all(db)
    .await?;

    let mut roles = Vec::with_capacity(rows.len());
    for row in rows {
        let name: String = row.try_get("name")?;
        let value: Value = row.try_get("value")?;
        let enabled: bool = row.try_get("enabled")?;
        roles.push(role_item_from_value(name, value, enabled));
    }
    Ok(roles)
}

async fn save_role(
    db: &PgPool,
    name: &str,
    value: &Value,
    enabled: bool,
) -> Result<(), sqlx_core::error::Error> {
    query(
        r#"
        INSERT INTO roles (name, value, enabled, updated_at)
        VALUES ($1, $2, $3, NOW())
        ON CONFLICT (name)
        DO UPDATE SET value = EXCLUDED.value,
                      enabled = EXCLUDED.enabled,
                      updated_at = NOW()
        "#,
    )
    .bind(name)
    .bind(value)
    .bind(enabled)
    .execute(db)
    .await?;
    Ok(())
}

async fn query_projects(
    db: &PgPool,
    q: &NormalizedProjectsQuery,
) -> Result<Vec<ProjectItem>, sqlx_core::error::Error> {
    let rows = query(
        r#"
        SELECT id, name, COALESCE(description, '') AS description, COALESCE(scope_json, '') AS scope_json,
               status, pinned, created_at::text AS created_at, updated_at::text AS updated_at
        FROM projects
        WHERE ($1 = '' OR status = $1)
          AND ($2 = '' OR name ILIKE $3 OR COALESCE(description, '') ILIKE $3)
        ORDER BY pinned DESC, updated_at DESC
        LIMIT $4 OFFSET $5
        "#,
    )
    .bind(&q.status)
    .bind(&q.search)
    .bind(q.search_pattern())
    .bind(q.limit)
    .bind(q.offset)
    .fetch_all(db)
    .await?;

    let mut projects = Vec::with_capacity(rows.len());
    for row in rows {
        projects.push(ProjectItem {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            description: row.try_get("description")?,
            scope_json: row.try_get("scope_json")?,
            status: row.try_get("status")?,
            pinned: row.try_get("pinned")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        });
    }
    Ok(projects)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedProjectInput {
    id: String,
    name: String,
    description: String,
    scope_json: String,
    status: String,
    pinned: bool,
    created_at: String,
    updated_at: String,
}

fn normalize_project_input(req: UpsertProjectRequest) -> Result<NormalizedProjectInput, ApiError> {
    let id = req.id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::bad_request("id is required"));
    }
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }
    let status = req
        .status
        .unwrap_or_else(|| "active".to_string())
        .trim()
        .to_string();
    let status = if status.is_empty() {
        "active".to_string()
    } else {
        status
    };
    Ok(NormalizedProjectInput {
        id,
        name,
        description: req.description.unwrap_or_default(),
        scope_json: req.scope_json.unwrap_or_default(),
        status,
        pinned: req.pinned.unwrap_or(false),
        created_at: req.created_at.unwrap_or_default().trim().to_string(),
        updated_at: req.updated_at.unwrap_or_default().trim().to_string(),
    })
}

async fn save_project(
    db: &PgPool,
    project: &NormalizedProjectInput,
) -> Result<(), sqlx_core::error::Error> {
    query(
        r#"
        INSERT INTO projects (id, name, description, scope_json, status, pinned, created_at, updated_at)
        VALUES (
            $1, $2, $3, $4, $5, $6,
            COALESCE(NULLIF($7, '')::timestamptz, NOW()),
            COALESCE(NULLIF($8, '')::timestamptz, NOW())
        )
        ON CONFLICT (id)
        DO UPDATE SET name = EXCLUDED.name,
                      description = EXCLUDED.description,
                      scope_json = EXCLUDED.scope_json,
                      status = EXCLUDED.status,
                      pinned = EXCLUDED.pinned,
                      updated_at = EXCLUDED.updated_at
        "#,
    )
    .bind(&project.id)
    .bind(&project.name)
    .bind(&project.description)
    .bind(&project.scope_json)
    .bind(&project.status)
    .bind(project.pinned)
    .bind(&project.created_at)
    .bind(&project.updated_at)
    .execute(db)
    .await?;
    Ok(())
}

async fn count_projects(
    db: &PgPool,
    q: &NormalizedProjectsQuery,
) -> Result<i64, sqlx_core::error::Error> {
    query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM projects
        WHERE ($1 = '' OR status = $1)
          AND ($2 = '' OR name ILIKE $3 OR COALESCE(description, '') ILIKE $3)
        "#,
    )
    .bind(&q.status)
    .bind(&q.search)
    .bind(q.search_pattern())
    .fetch_one(db)
    .await
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedConversationsQuery {
    limit: i64,
    offset: i64,
    search: String,
    sort_by: String,
}

impl ListConversationsQuery {
    fn normalized(self) -> NormalizedConversationsQuery {
        let mut limit = self.limit.unwrap_or(50);
        if limit <= 0 {
            limit = 50;
        }
        if limit > 1000 {
            limit = 1000;
        }
        let offset = self.offset.unwrap_or(0).max(0);
        let search = self.search.unwrap_or_default().trim().to_string();
        let sort_by = match self
            .sort_by
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "created_at" => "created_at".to_string(),
            _ => "updated_at".to_string(),
        };
        NormalizedConversationsQuery {
            limit,
            offset,
            search,
            sort_by,
        }
    }
}

impl NormalizedConversationsQuery {
    fn search_pattern(&self) -> String {
        if self.search.is_empty() {
            String::new()
        } else {
            format!("%{}%", self.search)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedConversationInput {
    id: String,
    title: String,
    project_id: String,
    pinned: bool,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedMessageInput {
    id: String,
    conversation_id: String,
    role: String,
    content: String,
    reasoning_content: String,
    mcp_execution_ids: Vec<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, PartialEq)]
struct NormalizedProcessDetailInput {
    id: String,
    message_id: String,
    conversation_id: String,
    event_type: String,
    message: String,
    data: Value,
    created_at: String,
}

async fn query_conversations(
    db: &PgPool,
    q: &NormalizedConversationsQuery,
) -> Result<Vec<ConversationItem>, sqlx_core::error::Error> {
    let rows = if q.search.is_empty() {
        let order_col = if q.sort_by == "created_at" {
            "created_at"
        } else {
            "updated_at"
        };
        let sql = format!(
            r#"
            SELECT id, title, COALESCE(project_id, '') AS project_id, pinned,
                   created_at::text AS created_at, updated_at::text AS updated_at
            FROM conversations
            ORDER BY {order_col} DESC
            LIMIT $1 OFFSET $2
            "#
        );
        query(&sql)
            .bind(q.limit)
            .bind(q.offset)
            .fetch_all(db)
            .await?
    } else {
        let order_col = if q.sort_by == "created_at" {
            "c.created_at"
        } else {
            "c.updated_at"
        };
        let sql = format!(
            r#"
            SELECT c.id, c.title, COALESCE(c.project_id, '') AS project_id, c.pinned,
                   c.created_at::text AS created_at, c.updated_at::text AS updated_at
            FROM conversations c
            WHERE c.title ILIKE $1
               OR EXISTS (
                   SELECT 1 FROM messages m
                   WHERE m.conversation_id = c.id AND m.content ILIKE $1
               )
            ORDER BY {order_col} DESC
            LIMIT $2 OFFSET $3
            "#
        );
        query(&sql)
            .bind(q.search_pattern())
            .bind(q.limit)
            .bind(q.offset)
            .fetch_all(db)
            .await?
    };

    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        items.push(conversation_item_from_row(row, Vec::new())?);
    }
    Ok(items)
}

async fn count_conversations(
    db: &PgPool,
    q: &NormalizedConversationsQuery,
) -> Result<i64, sqlx_core::error::Error> {
    if q.search.is_empty() {
        query_scalar::<_, i64>("SELECT COUNT(*) FROM conversations")
            .fetch_one(db)
            .await
    } else {
        query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM conversations c
            WHERE c.title ILIKE $1
               OR EXISTS (
                   SELECT 1 FROM messages m
                   WHERE m.conversation_id = c.id AND m.content ILIKE $1
               )
            "#,
        )
        .bind(q.search_pattern())
        .fetch_one(db)
        .await
    }
}

async fn query_conversation(
    db: &PgPool,
    id: &str,
    include_process_details: bool,
) -> Result<ConversationItem, ApiError> {
    let row = query(
        r#"
        SELECT id, title, COALESCE(project_id, '') AS project_id, pinned,
               created_at::text AS created_at, updated_at::text AS updated_at
        FROM conversations
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    let Some(row) = row else {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "对话不存在".to_string(),
        });
    };
    let messages = query_messages(db, id, include_process_details).await?;
    conversation_item_from_row(row, messages).map_err(ApiError::from)
}

fn conversation_item_from_row(
    row: sqlx_postgres::PgRow,
    messages: Vec<MessageItem>,
) -> Result<ConversationItem, sqlx_core::error::Error> {
    Ok(ConversationItem {
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        project_id: row.try_get("project_id")?,
        pinned: row.try_get("pinned")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        messages,
    })
}

async fn query_messages(
    db: &PgPool,
    conversation_id: &str,
    include_process_details: bool,
) -> Result<Vec<MessageItem>, sqlx_core::error::Error> {
    let rows = query(
        r#"
        SELECT id, conversation_id, role, content, COALESCE(reasoning_content, '') AS reasoning_content,
               mcp_execution_ids, created_at::text AS created_at, updated_at::text AS updated_at
        FROM messages
        WHERE conversation_id = $1
        ORDER BY created_at ASC, id ASC
        "#,
    )
    .bind(conversation_id)
    .fetch_all(db)
    .await?;

    let mut details_by_message: HashMap<String, Vec<ProcessDetailItem>> = HashMap::new();
    if include_process_details {
        details_by_message = query_process_details_by_conversation(db, conversation_id).await?;
    }

    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.try_get("id")?;
        let mcp_value: Value = row.try_get("mcp_execution_ids")?;
        let process_details = details_by_message.remove(&id).unwrap_or_default();
        items.push(MessageItem {
            id,
            conversation_id: row.try_get("conversation_id")?,
            role: row.try_get("role")?,
            content: row.try_get("content")?,
            reasoning_content: row.try_get("reasoning_content")?,
            mcp_execution_ids: json_string_array(mcp_value),
            process_details,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        });
    }
    Ok(items)
}

async fn query_process_details(
    db: &PgPool,
    message_id: &str,
) -> Result<Vec<ProcessDetailItem>, sqlx_core::error::Error> {
    let rows = query(
        r#"
        SELECT id, message_id, conversation_id, event_type, COALESCE(message, '') AS message,
               COALESCE(data, 'null'::jsonb) AS data, created_at::text AS created_at
        FROM process_details
        WHERE message_id = $1
        ORDER BY created_at ASC, id ASC
        "#,
    )
    .bind(message_id)
    .fetch_all(db)
    .await?;
    process_detail_items_from_rows(rows)
}

async fn query_process_details_by_conversation(
    db: &PgPool,
    conversation_id: &str,
) -> Result<HashMap<String, Vec<ProcessDetailItem>>, sqlx_core::error::Error> {
    let rows = query(
        r#"
        SELECT id, message_id, conversation_id, event_type, COALESCE(message, '') AS message,
               COALESCE(data, 'null'::jsonb) AS data, created_at::text AS created_at
        FROM process_details
        WHERE conversation_id = $1
        ORDER BY created_at ASC, id ASC
        "#,
    )
    .bind(conversation_id)
    .fetch_all(db)
    .await?;
    let items = process_detail_items_from_rows(rows)?;
    let mut grouped: HashMap<String, Vec<ProcessDetailItem>> = HashMap::new();
    for item in items {
        grouped
            .entry(item.message_id.clone())
            .or_default()
            .push(item);
    }
    Ok(grouped)
}

fn process_detail_items_from_rows(
    rows: Vec<sqlx_postgres::PgRow>,
) -> Result<Vec<ProcessDetailItem>, sqlx_core::error::Error> {
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        items.push(ProcessDetailItem {
            id: row.try_get("id")?,
            message_id: row.try_get("message_id")?,
            conversation_id: row.try_get("conversation_id")?,
            event_type: row.try_get("event_type")?,
            message: row.try_get("message")?,
            data: row.try_get("data")?,
            created_at: row.try_get("created_at")?,
        });
    }
    Ok(dedupe_process_details(items))
}

fn dedupe_process_details(items: Vec<ProcessDetailItem>) -> Vec<ProcessDetailItem> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let duplicate = out.last().map(|prev: &ProcessDetailItem| {
            prev.message_id == item.message_id
                && prev.event_type == item.event_type
                && prev.message == item.message
                && prev.data == item.data
        });
        if duplicate.unwrap_or(false) {
            continue;
        }
        out.push(item);
    }
    out
}

async fn conversation_exists(db: &PgPool, id: &str) -> Result<bool, sqlx_core::error::Error> {
    let exists =
        query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM conversations WHERE id = $1)")
            .bind(id)
            .fetch_one(db)
            .await?;
    Ok(exists)
}

async fn query_conversation_project_id(
    db: &PgPool,
    id: &str,
) -> Result<String, sqlx_core::error::Error> {
    Ok(query_scalar::<_, String>(
        "SELECT COALESCE(project_id, '') FROM conversations WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?
    .unwrap_or_default())
}

async fn ensure_project_exists(db: &PgPool, id: &str) -> Result<(), ApiError> {
    let exists = query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM projects WHERE id = $1)")
        .bind(id)
        .fetch_one(db)
        .await?;
    if exists {
        Ok(())
    } else {
        Err(ApiError::bad_request("项目不存在"))
    }
}

fn normalize_conversation_input(
    req: UpsertConversationRequest,
) -> Result<NormalizedConversationInput, ApiError> {
    let id = req.id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::bad_request("id is required"));
    }
    let title = req.title.trim().to_string();
    if title.is_empty() {
        return Err(ApiError::bad_request("title is required"));
    }
    Ok(NormalizedConversationInput {
        id,
        title,
        project_id: req.project_id.unwrap_or_default().trim().to_string(),
        pinned: req.pinned.unwrap_or(false),
        created_at: req.created_at.unwrap_or_default().trim().to_string(),
        updated_at: req.updated_at.unwrap_or_default().trim().to_string(),
    })
}

fn normalize_message_input(req: UpsertMessageRequest) -> Result<NormalizedMessageInput, ApiError> {
    let id = req.id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::bad_request("id is required"));
    }
    let conversation_id = req.conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Err(ApiError::bad_request("conversationId is required"));
    }
    let role = req.role.trim().to_string();
    if role.is_empty() {
        return Err(ApiError::bad_request("role is required"));
    }
    Ok(NormalizedMessageInput {
        id,
        conversation_id,
        role,
        content: req.content,
        reasoning_content: req.reasoning_content.unwrap_or_default(),
        mcp_execution_ids: clean_string_list(req.mcp_execution_ids.unwrap_or_default()),
        created_at: req.created_at.unwrap_or_default().trim().to_string(),
        updated_at: req.updated_at.unwrap_or_default().trim().to_string(),
    })
}

fn normalize_process_detail_input(
    req: UpsertProcessDetailRequest,
) -> Result<NormalizedProcessDetailInput, ApiError> {
    let id = req.id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::bad_request("id is required"));
    }
    let message_id = req.message_id.trim().to_string();
    if message_id.is_empty() {
        return Err(ApiError::bad_request("messageId is required"));
    }
    let conversation_id = req.conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Err(ApiError::bad_request("conversationId is required"));
    }
    let event_type = req.event_type.trim().to_string();
    if event_type.is_empty() {
        return Err(ApiError::bad_request("eventType is required"));
    }
    Ok(NormalizedProcessDetailInput {
        id,
        message_id,
        conversation_id,
        event_type,
        message: req.message.unwrap_or_default(),
        data: req.data.unwrap_or(Value::Null),
        created_at: req.created_at.unwrap_or_default().trim().to_string(),
    })
}

async fn save_conversation(
    db: &PgPool,
    conv: &NormalizedConversationInput,
) -> Result<(), sqlx_core::error::Error> {
    query(
        r#"
        INSERT INTO conversations (id, title, project_id, pinned, created_at, updated_at)
        VALUES (
            $1, $2, NULLIF($3, ''), $4,
            COALESCE(NULLIF($5, '')::timestamptz, NOW()),
            COALESCE(NULLIF($6, '')::timestamptz, NOW())
        )
        ON CONFLICT (id)
        DO UPDATE SET title = EXCLUDED.title,
                      project_id = EXCLUDED.project_id,
                      pinned = EXCLUDED.pinned,
                      updated_at = EXCLUDED.updated_at
        "#,
    )
    .bind(&conv.id)
    .bind(&conv.title)
    .bind(&conv.project_id)
    .bind(conv.pinned)
    .bind(&conv.created_at)
    .bind(&conv.updated_at)
    .execute(db)
    .await?;
    Ok(())
}

async fn save_message(
    db: &PgPool,
    msg: &NormalizedMessageInput,
) -> Result<(), sqlx_core::error::Error> {
    let ids = json!(msg.mcp_execution_ids);
    query(
        r#"
        INSERT INTO messages
            (id, conversation_id, role, content, reasoning_content, mcp_execution_ids, created_at, updated_at)
        VALUES (
            $1, $2, $3, $4, NULLIF($5, ''), $6,
            COALESCE(NULLIF($7, '')::timestamptz, NOW()),
            COALESCE(NULLIF($8, '')::timestamptz, NOW())
        )
        ON CONFLICT (id)
        DO UPDATE SET conversation_id = EXCLUDED.conversation_id,
                      role = EXCLUDED.role,
                      content = EXCLUDED.content,
                      reasoning_content = EXCLUDED.reasoning_content,
                      mcp_execution_ids = EXCLUDED.mcp_execution_ids,
                      updated_at = EXCLUDED.updated_at
        "#,
    )
    .bind(&msg.id)
    .bind(&msg.conversation_id)
    .bind(&msg.role)
    .bind(&msg.content)
    .bind(&msg.reasoning_content)
    .bind(&ids)
    .bind(&msg.created_at)
    .bind(&msg.updated_at)
    .execute(db)
    .await?;
    query(
        r#"
        UPDATE conversations
        SET updated_at = GREATEST(updated_at, COALESCE(NULLIF($2, '')::timestamptz, NOW()))
        WHERE id = $1
        "#,
    )
    .bind(&msg.conversation_id)
    .bind(&msg.updated_at)
    .execute(db)
    .await?;
    Ok(())
}

async fn save_process_detail(
    db: &PgPool,
    detail: &NormalizedProcessDetailInput,
) -> Result<(), sqlx_core::error::Error> {
    query(
        r#"
        INSERT INTO process_details
            (id, message_id, conversation_id, event_type, message, data, created_at)
        VALUES (
            $1, $2, $3, $4, $5, $6,
            COALESCE(NULLIF($7, '')::timestamptz, NOW())
        )
        ON CONFLICT (id)
        DO UPDATE SET message_id = EXCLUDED.message_id,
                      conversation_id = EXCLUDED.conversation_id,
                      event_type = EXCLUDED.event_type,
                      message = EXCLUDED.message,
                      data = EXCLUDED.data
        "#,
    )
    .bind(&detail.id)
    .bind(&detail.message_id)
    .bind(&detail.conversation_id)
    .bind(&detail.event_type)
    .bind(&detail.message)
    .bind(&detail.data)
    .bind(&detail.created_at)
    .execute(db)
    .await?;
    Ok(())
}

async fn save_process_detail_for_task_event(
    db: &PgPool,
    event_id: i64,
    detail: &NormalizedProcessDetailInput,
) -> Result<(), sqlx_core::error::Error> {
    query(
        r#"
        INSERT INTO process_details
            (id, message_id, conversation_id, event_type, message, data, created_at)
        SELECT $1, $2, $3, $4, $5, $6, created_at
        FROM agent_runtime_task_events
        WHERE id = $7
        ON CONFLICT (id)
        DO UPDATE SET message_id = EXCLUDED.message_id,
                      conversation_id = EXCLUDED.conversation_id,
                      event_type = EXCLUDED.event_type,
                      message = EXCLUDED.message,
                      data = EXCLUDED.data
        "#,
    )
    .bind(&detail.id)
    .bind(&detail.message_id)
    .bind(&detail.conversation_id)
    .bind(&detail.event_type)
    .bind(&detail.message)
    .bind(&detail.data)
    .bind(event_id)
    .execute(db)
    .await?;
    Ok(())
}

async fn query_agent_runtime_tasks(
    db: &PgPool,
) -> Result<Vec<AgentRuntimeTaskItem>, sqlx_core::error::Error> {
    let rows = query(
        r#"
        SELECT conversation_id,
               COALESCE(message, '') AS message,
               started_at::text AS started_at,
               status,
               COALESCE(agent_mode, '') AS agent_mode,
               COALESCE(assistant_message_id, '') AS assistant_message_id
        FROM agent_runtime_tasks
        WHERE active = TRUE
        ORDER BY started_at ASC
        "#,
    )
    .fetch_all(db)
    .await?;

    let mut tasks = Vec::with_capacity(rows.len());
    for row in rows {
        tasks.push(AgentRuntimeTaskItem {
            conversation_id: row.try_get("conversation_id")?,
            message: row.try_get("message")?,
            started_at: row.try_get("started_at")?,
            status: row.try_get("status")?,
            agent_mode: row.try_get("agent_mode")?,
            assistant_message_id: row.try_get("assistant_message_id")?,
        });
    }
    Ok(tasks)
}

async fn query_agent_runtime_task(
    db: &PgPool,
    conversation_id: &str,
) -> Result<Option<Value>, sqlx_core::error::Error> {
    let row = query(
        r#"
        SELECT conversation_id,
               COALESCE(message, '') AS message,
               started_at::text AS started_at,
               status,
               COALESCE(agent_mode, '') AS agent_mode,
               COALESCE(assistant_message_id, '') AS assistant_message_id,
               active,
               finished_at::text AS finished_at
        FROM agent_runtime_tasks
        WHERE conversation_id = $1
        "#,
    )
    .bind(conversation_id)
    .fetch_optional(db)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(json!({
        "conversationId": row.try_get::<String, _>("conversation_id")?,
        "message": row.try_get::<String, _>("message")?,
        "startedAt": row.try_get::<String, _>("started_at")?,
        "status": row.try_get::<String, _>("status")?,
        "agentMode": row.try_get::<String, _>("agent_mode")?,
        "assistantMessageId": row.try_get::<String, _>("assistant_message_id")?,
        "active": row.try_get::<bool, _>("active")?,
        "finishedAt": row.try_get::<Option<String>, _>("finished_at")?,
    })))
}

async fn query_runtime_todos(
    db: &PgPool,
    conversation_id: &str,
) -> Result<Vec<RuntimeTodoItem>, sqlx_core::error::Error> {
    let rows = query(
        r#"
        SELECT item_id, content, status, position, updated_at::text AS updated_at
        FROM agent_runtime_todos
        WHERE conversation_id = $1
        ORDER BY position ASC, item_id ASC
        "#,
    )
    .bind(conversation_id)
    .fetch_all(db)
    .await?;

    let mut todos = Vec::with_capacity(rows.len());
    for row in rows {
        todos.push(RuntimeTodoItem {
            item_id: row.try_get("item_id")?,
            content: row.try_get("content")?,
            status: row.try_get("status")?,
            position: row.try_get("position")?,
            updated_at: row.try_get("updated_at")?,
        });
    }
    Ok(todos)
}

async fn replace_runtime_todos(
    db: &PgPool,
    conversation_id: &str,
    items: &[RuntimeTodoItem],
) -> Result<Vec<RuntimeTodoItem>, sqlx_core::error::Error> {
    query("DELETE FROM agent_runtime_todos WHERE conversation_id = $1")
        .bind(conversation_id)
        .execute(db)
        .await?;
    for (position, item) in items.iter().enumerate() {
        query(
            r#"
            INSERT INTO agent_runtime_todos
                (conversation_id, item_id, content, status, position, updated_at)
            VALUES ($1, $2, $3, $4, $5, NOW())
            ON CONFLICT (conversation_id, item_id)
            DO UPDATE SET content = EXCLUDED.content,
                          status = EXCLUDED.status,
                          position = EXCLUDED.position,
                          updated_at = NOW()
            "#,
        )
        .bind(conversation_id)
        .bind(&item.item_id)
        .bind(&item.content)
        .bind(&item.status)
        .bind(position as i64)
        .execute(db)
        .await?;
    }
    query_runtime_todos(db, conversation_id).await
}

impl AgentRuntimeTaskEventsQuery {
    fn normalized(self, headers: &HeaderMap) -> NormalizedAgentRuntimeTaskEventsQuery {
        let last_event_id = headers
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let cursor_candidates = [
            last_event_id.as_deref(),
            self.after_event_id.as_deref(),
            self.runtime_event_id.as_deref(),
        ];
        let after_id = cursor_candidates
            .iter()
            .filter_map(|candidate| candidate.and_then(parse_agent_runtime_task_event_after_id))
            .next()
            .unwrap_or(0);
        let after_runtime_event_id = if after_id > 0 {
            String::new()
        } else {
            cursor_candidates
                .iter()
                .filter_map(|candidate| candidate.map(str::trim))
                .find(|candidate| !candidate.is_empty())
                .unwrap_or_default()
                .to_string()
        };
        let conversation_id = self.conversation_id.unwrap_or_default().trim().to_string();
        NormalizedAgentRuntimeTaskEventsQuery {
            scoped_to_conversation: !conversation_id.is_empty(),
            conversation_id,
            after_id,
            after_runtime_event_id,
            limit: self.limit.unwrap_or(100).clamp(1, 1000),
        }
    }
}

fn parse_agent_runtime_task_event_after_id(raw: &str) -> Option<i64> {
    let value = raw.trim().strip_prefix("id:").unwrap_or(raw.trim()).trim();
    if value.is_empty() {
        return None;
    }
    value.parse::<i64>().ok().filter(|id| *id > 0)
}

fn normalize_agent_runtime_task_event_input(
    req: CreateAgentRuntimeTaskEventRequest,
) -> Result<NormalizedAgentRuntimeTaskEventInput, ApiError> {
    let conversation_id = req.conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Err(ApiError::bad_request("conversationId is required"));
    }
    let line = normalize_sse_line(req.line);
    if line.trim().is_empty() {
        return Err(ApiError::bad_request("line is required"));
    }
    Ok(NormalizedAgentRuntimeTaskEventInput {
        conversation_id,
        line,
        runtime_event_id: req
            .runtime_event_id
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        event_type: req
            .event_type
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        terminal: req.terminal.unwrap_or(false),
    })
}

fn normalize_sse_line(line: String) -> String {
    let mut line = line
        .trim_end_matches(|c| c == '\r' || c == '\n')
        .to_string();
    line.push_str("\n\n");
    line
}

async fn save_and_publish_agent_runtime_task_event(
    db: &PgPool,
    bus: &AgentRuntimeTaskEventBus,
    event: &NormalizedAgentRuntimeTaskEventInput,
) -> Result<i64, sqlx_core::error::Error> {
    let (id, inserted) = save_agent_runtime_task_event(db, event).await?;
    if inserted {
        let _ = bus.send(StoredAgentRuntimeTaskEvent {
            id,
            conversation_id: event.conversation_id.clone(),
            event_type: event.event_type.clone(),
            line: event.line.clone(),
            terminal: event.terminal,
        });
        if !is_global_state_task_event_type(&event.event_type) {
            if let Some(items) = runtime_todos_from_task_event(event) {
                persist_and_publish_runtime_todos(db, bus, &event.conversation_id, &items).await?;
            }
        }
    }
    Ok(id)
}

async fn save_and_publish_state_event(
    db: &PgPool,
    bus: &AgentRuntimeTaskEventBus,
    event: NormalizedAgentRuntimeTaskEventInput,
) -> Result<i64, sqlx_core::error::Error> {
    let (id, inserted) = save_agent_runtime_task_event(db, &event).await?;
    if inserted {
        let _ = bus.send(StoredAgentRuntimeTaskEvent {
            id,
            conversation_id: event.conversation_id,
            event_type: event.event_type,
            line: event.line,
            terminal: event.terminal,
        });
    }
    Ok(id)
}

async fn persist_and_publish_runtime_todos(
    db: &PgPool,
    bus: &AgentRuntimeTaskEventBus,
    conversation_id: &str,
    items: &[RuntimeTodoItem],
) -> Result<(), sqlx_core::error::Error> {
    let todos = replace_runtime_todos(db, conversation_id, items).await?;
    let event = runtime_todo_updated_event(conversation_id, &todos);
    let _ = save_and_publish_state_event(db, bus, event).await?;
    Ok(())
}

fn runtime_todo_updated_event(
    conversation_id: &str,
    todos: &[RuntimeTodoItem],
) -> NormalizedAgentRuntimeTaskEventInput {
    let conversation_id = conversation_id.trim().to_string();
    NormalizedAgentRuntimeTaskEventInput {
        conversation_id: conversation_id.clone(),
        line: normalize_sse_line(format!(
            "data: {}",
            json!({
                "type": "todo_updated",
                "message": "Todo state updated",
                "data": {
                    "conversationId": conversation_id,
                    "todos": todos,
                }
            })
        )),
        runtime_event_id: String::new(),
        event_type: "todo_updated".to_string(),
        terminal: false,
    }
}

async fn save_agent_runtime_task_event(
    db: &PgPool,
    event: &NormalizedAgentRuntimeTaskEventInput,
) -> Result<(i64, bool), sqlx_core::error::Error> {
    let (id, inserted) = if !event.runtime_event_id.is_empty() {
        if let Some(id) = query_scalar::<_, i64>(
            r#"
            SELECT id
            FROM agent_runtime_task_events
            WHERE conversation_id = $1 AND runtime_event_id = $2 AND event_type = $3
            ORDER BY id ASC
            LIMIT 1
            "#,
        )
        .bind(&event.conversation_id)
        .bind(&event.runtime_event_id)
        .bind(&event.event_type)
        .fetch_optional(db)
        .await?
        {
            (id, false)
        } else {
            let id = query_scalar::<_, i64>(
                r#"
                INSERT INTO agent_runtime_task_events
                    (conversation_id, runtime_event_id, event_type, sse_line, terminal, created_at)
                VALUES ($1, $2, $3, $4, $5, NOW())
                RETURNING id
                "#,
            )
            .bind(&event.conversation_id)
            .bind(&event.runtime_event_id)
            .bind(&event.event_type)
            .bind(&event.line)
            .bind(event.terminal)
            .fetch_one(db)
            .await?;
            (id, true)
        }
    } else {
        let id = query_scalar::<_, i64>(
            r#"
            INSERT INTO agent_runtime_task_events
                (conversation_id, runtime_event_id, event_type, sse_line, terminal, created_at)
            VALUES ($1, $2, $3, $4, $5, NOW())
            RETURNING id
            "#,
        )
        .bind(&event.conversation_id)
        .bind(&event.runtime_event_id)
        .bind(&event.event_type)
        .bind(&event.line)
        .bind(event.terminal)
        .fetch_one(db)
        .await?;
        (id, true)
    };
    if inserted {
        let _ = mirror_agent_runtime_task_event_to_process_detail(db, id, event).await;
    }
    Ok((id, inserted))
}

async fn mirror_agent_runtime_task_event_to_process_detail(
    db: &PgPool,
    event_id: i64,
    event: &NormalizedAgentRuntimeTaskEventInput,
) -> Result<(), sqlx_core::error::Error> {
    let assistant_message_id = query_scalar::<_, String>(
        r#"
        SELECT COALESCE(
            (SELECT assistant_message_id
             FROM agent_runtime_tasks
             WHERE conversation_id = $1
               AND assistant_message_id IS NOT NULL
               AND assistant_message_id <> ''
             LIMIT 1),
            (SELECT assistant_message_id
             FROM agent_runtime_stream_runs
             WHERE conversation_id = $1
               AND assistant_message_id IS NOT NULL
               AND assistant_message_id <> ''
             ORDER BY created_at DESC
             LIMIT 1),
            ''
        )
        "#,
    )
    .bind(&event.conversation_id)
    .fetch_one(db)
    .await?;
    let assistant_message_id = assistant_message_id.trim().to_string();
    if assistant_message_id.is_empty() {
        return Ok(());
    }
    let message_exists = query_scalar::<_, bool>(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM messages
            WHERE id = $1 AND conversation_id = $2
        )
        "#,
    )
    .bind(&assistant_message_id)
    .bind(&event.conversation_id)
    .fetch_one(db)
    .await?;
    if !message_exists {
        return Ok(());
    }
    if let Some(detail) =
        agent_runtime_process_detail_from_task_event(event_id, event, &assistant_message_id)
    {
        save_process_detail_for_task_event(db, event_id, &detail).await?;
    }
    Ok(())
}

fn agent_runtime_process_detail_from_task_event(
    event_id: i64,
    event: &NormalizedAgentRuntimeTaskEventInput,
    assistant_message_id: &str,
) -> Option<NormalizedProcessDetailInput> {
    let envelope = sse_line_json(&event.line)?;
    let outer_type = json_string_field(&envelope, "type")
        .or_else(|| json_string_field(&envelope, "event"))
        .unwrap_or_default();
    if matches!(
        outer_type.as_str(),
        "message_saved"
            | "conversation_title_updated"
            | "done"
            | "task_updated"
            | "task_completed"
            | "task_removed"
            | "hitl_pending_updated"
            | "hitl_decision_updated"
            | "todo_updated"
            | "todo_cleared"
    ) {
        return None;
    }

    let data_value = envelope.get("data").cloned().unwrap_or(Value::Null);
    let mut data = json_object_from_value(data_value);
    let trace = data.get("runtimeTrace").cloned().unwrap_or(Value::Null);
    let runtime_event_type = json_string_field_value(&data, "runtimeEventType")
        .or_else(|| json_string_field(&trace, "event"))
        .or_else(|| json_string_field(&trace, "type"))
        .or_else(|| {
            if event.event_type.trim().is_empty() {
                None
            } else {
                Some(event.event_type.trim().to_string())
            }
        })
        .unwrap_or_default();
    let event_type = if outer_type.trim().is_empty() {
        runtime_event_to_frontend_type(&runtime_event_type).to_string()
    } else {
        outer_type
    };
    if matches!(
        event_type.as_str(),
        "message_saved"
            | "conversation_title_updated"
            | "done"
            | "task_updated"
            | "task_completed"
            | "task_removed"
            | "hitl_pending_updated"
            | "hitl_decision_updated"
            | "todo_updated"
            | "todo_cleared"
    ) {
        return None;
    }

    insert_string_if_absent(&mut data, "conversationId", &event.conversation_id);
    insert_string_if_absent(&mut data, "assistantMessageId", assistant_message_id);
    insert_string_if_absent(&mut data, "source", "agent_runtime");
    if !runtime_event_type.is_empty() {
        insert_string_if_absent(&mut data, "runtimeEventType", &runtime_event_type);
    }
    enrich_agent_runtime_process_detail_data(&mut data, &trace);

    let message = json_string_field(&envelope, "message")
        .or_else(|| json_string_field_value(&data, "message"))
        .or_else(|| json_string_field(&trace, "message"))
        .or_else(|| json_string_field(&trace, "delta"))
        .or_else(|| json_string_field(&trace, "response"))
        .unwrap_or_default();

    Some(NormalizedProcessDetailInput {
        id: format!("agent-runtime-task-event-{event_id}"),
        message_id: assistant_message_id.trim().to_string(),
        conversation_id: event.conversation_id.clone(),
        event_type,
        message,
        data: Value::Object(data),
        created_at: String::new(),
    })
}

fn sse_line_json(line: &str) -> Option<Value> {
    let mut data_lines = Vec::new();
    for raw in line.replace("\r\n", "\n").split('\n') {
        let trimmed = raw.trim();
        if let Some(data) = trimmed.strip_prefix("data:") {
            data_lines.push(data.trim().to_string());
        }
    }
    let data = data_lines.join("\n");
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        return None;
    }
    serde_json::from_str::<Value>(&data).ok()
}

fn json_object_from_value(value: Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("value".to_string(), other);
            map
        }
    }
}

fn json_string_field(value: &Value, key: &str) -> Option<String> {
    value
        .as_object()
        .and_then(|map| json_string_field_value(map, key))
}

fn json_string_field_value(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    match map.get(key) {
        Some(Value::String(value)) => {
            let value = value.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        }
        Some(Value::Number(value)) => Some(value.to_string()),
        _ => None,
    }
}

fn insert_string_if_absent(map: &mut serde_json::Map<String, Value>, key: &str, value: &str) {
    if value.trim().is_empty() || map.contains_key(key) {
        return;
    }
    map.insert(key.to_string(), Value::String(value.trim().to_string()));
}

fn insert_value_if_absent(
    map: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Option<Value>,
) {
    if map.contains_key(key) {
        return;
    }
    if let Some(value) = value {
        if !value.is_null() {
            map.insert(key.to_string(), value);
        }
    }
}

fn cloned_trace_field(trace: &Value, keys: &[&str]) -> Option<Value> {
    let obj = trace.as_object()?;
    for key in keys {
        if let Some(value) = obj.get(*key) {
            if !value.is_null() {
                return Some(value.clone());
            }
        }
    }
    None
}

fn cloned_nested_trace_field(trace: &Value, object_key: &str, keys: &[&str]) -> Option<Value> {
    let nested = trace.as_object()?.get(object_key)?.as_object()?;
    for key in keys {
        if let Some(value) = nested.get(*key) {
            if !value.is_null() {
                return Some(value.clone());
            }
        }
    }
    None
}

fn trace_string(trace: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = json_string_field(trace, key) {
            return Some(value);
        }
    }
    None
}

fn nested_trace_string(trace: &Value, object_key: &str, keys: &[&str]) -> Option<String> {
    let nested = trace.as_object()?.get(object_key)?;
    for key in keys {
        if let Some(value) = json_string_field(nested, key) {
            return Some(value);
        }
    }
    None
}

fn enrich_agent_runtime_process_detail_data(
    data: &mut serde_json::Map<String, Value>,
    trace: &Value,
) {
    if let Some(value) = trace_string(trace, &["turnId", "turn_id"]) {
        insert_string_if_absent(data, "turnId", &value);
    }
    if let Some(value) = trace_string(trace, &["assistantMessageId", "assistant_message_id"]) {
        insert_string_if_absent(data, "assistantMessageId", &value);
    }
    if let Some(value) = trace_string(trace, &["toolCallId", "toolCallID", "tool_call_id"]) {
        insert_string_if_absent(data, "toolCallId", &value);
    } else if let Some(value) = nested_trace_string(
        trace,
        "tool",
        &["callId", "toolCallId", "toolCallID", "tool_call_id"],
    ) {
        insert_string_if_absent(data, "toolCallId", &value);
    }
    if let Some(value) = trace_string(trace, &["toolName", "tool_name", "name"]) {
        insert_string_if_absent(data, "toolName", &value);
    } else if let Some(value) = nested_trace_string(
        trace,
        "tool",
        &["identity", "name", "toolName", "tool_name", "mcpName"],
    ) {
        insert_string_if_absent(data, "toolName", &value);
    }
    insert_value_if_absent(
        data,
        "argumentsObj",
        cloned_trace_field(trace, &["arguments", "args"])
            .or_else(|| cloned_nested_trace_field(trace, "tool", &["arguments", "args"])),
    );
    insert_value_if_absent(
        data,
        "result",
        cloned_trace_field(trace, &["result"])
            .or_else(|| cloned_nested_trace_field(trace, "tool", &["result"])),
    );
    insert_value_if_absent(
        data,
        "error",
        cloned_trace_field(trace, &["error"])
            .or_else(|| cloned_nested_trace_field(trace, "tool", &["error"])),
    );
    insert_value_if_absent(data, "delta", cloned_trace_field(trace, &["delta"]));
    insert_value_if_absent(data, "response", cloned_trace_field(trace, &["response"]));
    insert_value_if_absent(
        data,
        "accumulated",
        cloned_trace_field(trace, &["accumulated", "__sse_accumulated"]),
    );
    insert_value_if_absent(data, "items", cloned_trace_field(trace, &["items", "plan"]));
    insert_value_if_absent(data, "plan", cloned_trace_field(trace, &["plan"]));
}

fn runtime_todos_from_task_event(
    event: &NormalizedAgentRuntimeTaskEventInput,
) -> Option<Vec<RuntimeTodoItem>> {
    let envelope = sse_line_json(&event.line)?;
    let data = envelope.get("data").cloned().unwrap_or(Value::Null);
    let trace = data
        .as_object()
        .and_then(|obj| obj.get("runtimeTrace"))
        .cloned()
        .unwrap_or(Value::Null);
    let outer_type = json_string_field(&envelope, "type")
        .or_else(|| json_string_field(&envelope, "event"))
        .unwrap_or_default();
    let runtime_type = json_string_field(&data, "runtimeEventType")
        .or_else(|| json_string_field(&trace, "type"))
        .or_else(|| json_string_field(&trace, "event"))
        .or_else(|| {
            let value = event.event_type.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        })
        .unwrap_or_default()
        .to_ascii_lowercase();
    let tool_name = json_string_field(&trace, "toolName")
        .or_else(|| json_string_field(&trace, "tool_name"))
        .or_else(|| nested_trace_string(&trace, "tool", &["name", "identity", "toolName"]))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let recognized = matches!(
        runtime_type.as_str(),
        "plan_updated" | "planning" | "todowrite" | "todo_write" | "update_plan" | "update_todo"
    ) || outer_type == "planning"
        || matches!(
            tool_name.as_str(),
            "todowrite" | "todo_write" | "update_plan"
        );
    if !recognized {
        return None;
    }

    let mut candidates = Vec::new();
    collect_todo_candidates(&mut candidates, &trace);
    collect_todo_candidates(&mut candidates, &data);
    if candidates.is_empty() {
        if let Some(message) = json_string_field(&envelope, "message") {
            let parsed = parse_markdown_todos(&message);
            if !parsed.is_empty() {
                return Some(parsed);
            }
        }
        return None;
    }
    for candidate in candidates {
        if let Some(items) = normalize_runtime_todo_items(&candidate) {
            return Some(items);
        }
    }
    None
}

fn collect_todo_candidates(out: &mut Vec<Value>, value: &Value) {
    let Some(obj) = value.as_object() else {
        return;
    };
    for key in ["items", "plan", "todos", "todoItems"] {
        if let Some(candidate) = obj.get(key) {
            out.push(candidate.clone());
        }
    }
    for key in ["arguments", "args", "input", "payload"] {
        if let Some(candidate) = obj.get(key) {
            collect_todo_candidates(out, candidate);
        }
    }
    if let Some(tool) = obj.get("tool") {
        collect_todo_candidates(out, tool);
    }
}

fn normalize_runtime_todo_items(raw: &Value) -> Option<Vec<RuntimeTodoItem>> {
    if raw.is_null() {
        return None;
    }
    if let Some(text) = raw.as_str() {
        let items = parse_markdown_todos(text);
        return if items.is_empty() { None } else { Some(items) };
    }
    let array = raw.as_array()?;
    let mut items = Vec::with_capacity(array.len());
    for (index, item) in array.iter().enumerate() {
        let todo = runtime_todo_item_from_value(item, index as i64)?;
        items.push(todo);
    }
    Some(items)
}

fn runtime_todo_item_from_value(raw: &Value, position: i64) -> Option<RuntimeTodoItem> {
    if let Some(text) = raw.as_str() {
        let content = text.trim();
        if content.is_empty() {
            return None;
        }
        return Some(RuntimeTodoItem {
            item_id: format!("todo-{position}"),
            content: content.to_string(),
            status: "pending".to_string(),
            position,
            updated_at: String::new(),
        });
    }
    let obj = raw.as_object()?;
    let content = json_string_field_value(obj, "content")
        .or_else(|| json_string_field_value(obj, "task"))
        .or_else(|| json_string_field_value(obj, "step"))
        .or_else(|| json_string_field_value(obj, "text"))
        .or_else(|| json_string_field_value(obj, "title"))
        .unwrap_or_default();
    let content = content.trim().to_string();
    if content.is_empty() {
        return None;
    }
    let item_id = json_string_field_value(obj, "id")
        .or_else(|| json_string_field_value(obj, "itemId"))
        .or_else(|| json_string_field_value(obj, "item_id"))
        .unwrap_or_else(|| format!("todo-{position}"));
    let status = json_string_field_value(obj, "status")
        .and_then(|value| normalize_runtime_todo_status(&value))
        .unwrap_or_else(|| "pending".to_string());
    Some(RuntimeTodoItem {
        item_id,
        content,
        status,
        position,
        updated_at: String::new(),
    })
}

fn normalize_runtime_todo_status(status: &str) -> Option<String> {
    match status.trim().to_ascii_lowercase().as_str() {
        "pending" | "todo" | "open" => Some("pending".to_string()),
        "in_progress" | "inprogress" | "running" | "active" | "doing" => {
            Some("in_progress".to_string())
        }
        "completed" | "complete" | "done" | "checked" => Some("completed".to_string()),
        "cancelled" | "canceled" | "skipped" => Some("cancelled".to_string()),
        _ => None,
    }
}

fn parse_markdown_todos(text: &str) -> Vec<RuntimeTodoItem> {
    let mut items = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        let Some((status, content)) = parse_markdown_todo_line(line) else {
            continue;
        };
        let position = items.len() as i64;
        items.push(RuntimeTodoItem {
            item_id: format!("todo-{line_index}"),
            content,
            status,
            position,
            updated_at: String::new(),
        });
    }
    items
}

fn parse_markdown_todo_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    let after_marker = trimmed
        .strip_prefix("-")
        .or_else(|| trimmed.strip_prefix("*"))
        .map(str::trim_start)
        .or_else(|| {
            let idx = trimmed.find(|c| c == '.' || c == ')')?;
            let (digits, rest) = trimmed.split_at(idx);
            if digits.chars().all(|c| c.is_ascii_digit()) {
                Some(rest[1..].trim_start())
            } else {
                None
            }
        })?;
    let rest = after_marker.strip_prefix('[')?;
    let close = rest.find(']')?;
    let marker = rest[..close].trim().to_ascii_lowercase();
    let content = rest[close + 1..].trim().to_string();
    if content.is_empty() {
        return None;
    }
    let status = match marker.as_str() {
        "" => "pending".to_string(),
        "x" | "✓" | "done" => "completed".to_string(),
        "-" | "~" | "cancelled" | "canceled" => "cancelled".to_string(),
        ">" | "*" | "in_progress" | "doing" => "in_progress".to_string(),
        _ => normalize_runtime_todo_status(&marker)?,
    };
    Some((status, content))
}

async fn query_agent_runtime_task_events(
    db: &PgPool,
    q: &NormalizedAgentRuntimeTaskEventsQuery,
) -> Result<Vec<StoredAgentRuntimeTaskEvent>, sqlx_core::error::Error> {
    let rows = if q.scoped_to_conversation {
        query(
            r#"
            SELECT id, conversation_id, event_type, sse_line, terminal
            FROM agent_runtime_task_events
            WHERE id > $1
              AND conversation_id = $2
            ORDER BY id ASC
            LIMIT $3
            "#,
        )
        .bind(q.after_id)
        .bind(&q.conversation_id)
        .bind(q.limit)
        .fetch_all(db)
        .await?
    } else {
        query(
            r#"
            SELECT e.id, e.conversation_id, e.event_type, e.sse_line, e.terminal
            FROM agent_runtime_task_events e
            WHERE e.id > $1
              AND (
                  e.terminal = TRUE
                  OR e.event_type IN (
                      'task_updated',
                      'task_completed',
                      'task_removed',
                      'hitl_pending_updated',
                      'hitl_decision_updated',
                      'todo_updated',
                      'todo_cleared'
                  )
                  OR EXISTS (
                      SELECT 1
                      FROM agent_runtime_tasks t
                      WHERE t.conversation_id = e.conversation_id
                        AND t.active = TRUE
                  )
              )
            ORDER BY e.id ASC
            LIMIT $2
            "#,
        )
        .bind(q.after_id)
        .bind(q.limit)
        .fetch_all(db)
        .await?
    };

    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        events.push(StoredAgentRuntimeTaskEvent {
            id: row.try_get("id")?,
            conversation_id: row.try_get("conversation_id")?,
            event_type: row.try_get("event_type")?,
            line: row.try_get("sse_line")?,
            terminal: row.try_get("terminal")?,
        });
    }
    Ok(events)
}

async fn resolve_agent_runtime_task_event_after_id(
    db: &PgPool,
    q: &NormalizedAgentRuntimeTaskEventsQuery,
) -> Result<i64, sqlx_core::error::Error> {
    if q.after_id > 0 || q.after_runtime_event_id.trim().is_empty() {
        return Ok(q.after_id);
    }
    let runtime_event_id = q.after_runtime_event_id.trim();
    let id = if q.scoped_to_conversation {
        query_scalar::<_, i64>(
            r#"
            SELECT id
            FROM agent_runtime_task_events
            WHERE conversation_id = $1 AND runtime_event_id = $2
            ORDER BY id DESC
            LIMIT 1
            "#,
        )
        .bind(&q.conversation_id)
        .bind(runtime_event_id)
        .fetch_optional(db)
        .await?
    } else {
        query_scalar::<_, i64>(
            r#"
            SELECT id
            FROM agent_runtime_task_events
            WHERE runtime_event_id = $1
            ORDER BY id DESC
            LIMIT 1
            "#,
        )
        .bind(runtime_event_id)
        .fetch_optional(db)
        .await?
    };
    Ok(id.unwrap_or(0).max(0))
}

async fn query_agent_runtime_final_response(
    db: &PgPool,
    conversation_id: &str,
) -> Result<String, sqlx_core::error::Error> {
    let rows = query(
        r#"
        SELECT sse_line
        FROM agent_runtime_task_events
        WHERE conversation_id = $1
        ORDER BY id ASC
        "#,
    )
    .bind(conversation_id)
    .fetch_all(db)
    .await?;

    let lines: Result<Vec<String>, sqlx_core::error::Error> = rows
        .into_iter()
        .map(|row| row.try_get("sse_line"))
        .collect();
    Ok(agent_runtime_final_response_from_sse_lines(
        lines?.iter().map(String::as_str),
    ))
}

async fn query_agent_runtime_final_response_and_reasoning(
    db: &PgPool,
    conversation_id: &str,
) -> Result<(String, String), sqlx_core::error::Error> {
    let rows = query(
        r#"
        SELECT sse_line
        FROM agent_runtime_task_events
        WHERE conversation_id = $1
        ORDER BY id ASC
        "#,
    )
    .bind(conversation_id)
    .fetch_all(db)
    .await?;

    let lines: Result<Vec<String>, sqlx_core::error::Error> = rows
        .into_iter()
        .map(|row| row.try_get("sse_line"))
        .collect();
    let lines = lines?;
    let response = agent_runtime_final_response_from_sse_lines(lines.iter().map(String::as_str));
    let reasoning = agent_runtime_reasoning_from_sse_lines(lines.iter().map(String::as_str));
    Ok((response, reasoning))
}

async fn finalize_agent_runtime_assistant_message(
    db: &PgPool,
    conversation_id: &str,
    assistant_message_id: &str,
    response: &str,
    reasoning: &str,
) -> Result<(), sqlx_core::error::Error> {
    let assistant_message_id = assistant_message_id.trim();
    if assistant_message_id.is_empty() {
        return Ok(());
    }
    let content = {
        let trimmed = response.trim();
        if trimmed.is_empty() {
            "Agent Runtime 已完成，但未返回助手正文。"
        } else {
            trimmed
        }
    };
    query(
        r#"
        UPDATE messages
        SET content = $1,
            reasoning_content = NULLIF($2, ''),
            updated_at = NOW()
        WHERE id = $3 AND conversation_id = $4
        "#,
    )
    .bind(content)
    .bind(reasoning.trim())
    .bind(assistant_message_id)
    .bind(conversation_id.trim())
    .execute(db)
    .await?;
    Ok(())
}

fn agent_runtime_reasoning_from_sse_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    let mut reasoning = String::new();
    for line in lines {
        if let Some(delta) = agent_runtime_reasoning_delta_from_sse_line(line) {
            reasoning.push_str(&delta);
        }
    }
    reasoning
}

fn agent_runtime_reasoning_delta_from_sse_line(line: &str) -> Option<String> {
    let envelope = sse_line_json(line)?;
    let event_type = json_string_field(&envelope, "type")
        .or_else(|| json_string_field(&envelope, "event"))
        .unwrap_or_default();
    let message = json_string_field(&envelope, "message").unwrap_or_default();
    let data = envelope.get("data").cloned().unwrap_or(Value::Null);
    let trace = data
        .as_object()
        .and_then(|map| map.get("runtimeTrace"))
        .cloned()
        .unwrap_or(Value::Null);
    let runtime_event_type = json_string_field(&data, "runtimeEventType")
        .or_else(|| json_string_field(&trace, "event"))
        .or_else(|| json_string_field(&trace, "type"))
        .unwrap_or_else(|| event_type.clone());
    if runtime_event_type != "reasoning_delta" && event_type != "reasoning_chain_stream_delta" {
        return None;
    }
    json_string_field(&trace, "delta")
        .or_else(|| json_string_field(&data, "delta"))
        .or_else(|| {
            if message.is_empty() {
                None
            } else {
                Some(message)
            }
        })
}

fn agent_runtime_final_response_from_sse_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut latest_response = String::new();
    let mut latest_accumulated = String::new();
    let mut latest_completed = String::new();
    let mut turn_response = String::new();
    let mut turn_accumulated = String::new();
    for line in lines {
        if let Some(result) = agent_runtime_final_response_from_sse_line(line) {
            if result.starts_turn {
                turn_response.clear();
                turn_accumulated.clear();
            }
            if !result.response.is_empty() {
                latest_response = result.response;
                turn_response = latest_response.clone();
            }
            if !result.accumulated.is_empty() {
                latest_accumulated = result.accumulated;
                turn_accumulated = latest_accumulated.clone();
            }
            if result.completed {
                if !turn_response.is_empty() {
                    latest_completed = turn_response.clone();
                } else if !turn_accumulated.is_empty() {
                    latest_completed = turn_accumulated.clone();
                }
            }
        }
    }
    if !latest_completed.is_empty() {
        latest_completed
    } else if !latest_response.is_empty() {
        latest_response
    } else {
        latest_accumulated
    }
}

#[derive(Default)]
struct AgentRuntimeFinalResponseCandidate {
    response: String,
    accumulated: String,
    completed: bool,
    starts_turn: bool,
}

fn agent_runtime_final_response_from_sse_line(
    line: &str,
) -> Option<AgentRuntimeFinalResponseCandidate> {
    let envelope = sse_line_json(line)?;
    let event_type = json_string_field(&envelope, "type")
        .or_else(|| json_string_field(&envelope, "event"))
        .unwrap_or_default();
    let message = json_string_field(&envelope, "message").unwrap_or_default();
    let data = envelope.get("data").cloned().unwrap_or(Value::Null);
    let trace = data
        .as_object()
        .and_then(|map| map.get("runtimeTrace"))
        .cloned()
        .unwrap_or(Value::Null);
    let runtime_event_type = json_string_field(&data, "runtimeEventType")
        .or_else(|| json_string_field(&data, "runtime_event_type"))
        .or_else(|| json_string_field(&trace, "event"))
        .or_else(|| json_string_field(&trace, "type"))
        .unwrap_or_else(|| event_type.clone());

    let mut candidate = AgentRuntimeFinalResponseCandidate {
        completed: matches!(runtime_event_type.as_str(), "turn_completed" | "done")
            || matches!(event_type.as_str(), "response" | "done"),
        starts_turn: matches!(
            runtime_event_type.as_str(),
            "session_started" | "turn_started"
        ),
        ..Default::default()
    };
    if event_type == "response" && !message.is_empty() {
        candidate.response = message.clone();
    }
    if runtime_event_type == "turn_completed"
        && !message.is_empty()
        && message != "Agent Runtime turn 已完成"
    {
        candidate.response = message;
    }
    if let Some(response) =
        json_string_field(&data, "response").or_else(|| json_string_field(&trace, "response"))
    {
        candidate.response = response;
    }
    if let Some(accumulated) = json_string_field(&data, "accumulated")
        .or_else(|| json_string_field(&data, "__sse_accumulated"))
        .or_else(|| json_string_field(&trace, "accumulated"))
        .or_else(|| json_string_field(&trace, "__sse_accumulated"))
    {
        candidate.accumulated = accumulated;
    }
    Some(candidate)
}

fn agent_runtime_task_events_sse_response(
    db: PgPool,
    bus: AgentRuntimeTaskEventBus,
    query: NormalizedAgentRuntimeTaskEventsQuery,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::convert::Infallible>>(128);
    tokio::spawn(async move {
        stream_agent_runtime_task_events_to_channel(db, bus, query, tx).await;
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(
            "content-type",
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        )
        .header(
            "cache-control",
            HeaderValue::from_static("no-cache, no-transform"),
        )
        .header("connection", HeaderValue::from_static("keep-alive"))
        .header("x-accel-buffering", HeaderValue::from_static("no"))
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .expect("valid SSE response")
}

async fn stream_agent_runtime_task_events_to_channel(
    db: PgPool,
    bus: AgentRuntimeTaskEventBus,
    query: NormalizedAgentRuntimeTaskEventsQuery,
    tx: mpsc::Sender<Result<Bytes, std::convert::Infallible>>,
) {
    let mut query = query;
    let mut last_sent_id = match resolve_agent_runtime_task_event_after_id(&db, &query).await {
        Ok(id) => id,
        Err(err) => {
            let frame = agent_runtime_task_events_error_sse_frame(&err.to_string());
            let _ = tx.send(Ok(Bytes::from(frame))).await;
            return;
        }
    };
    query.after_id = last_sent_id;
    let mut rx = bus.subscribe();
    match replay_agent_runtime_task_events(&db, &query, last_sent_id, &tx).await {
        Ok(next_id) => last_sent_id = next_id,
        Err(StreamTaskEventsError::Database(err)) => {
            let frame = agent_runtime_task_events_error_sse_frame(&err.to_string());
            let _ = tx.send(Ok(Bytes::from(frame))).await;
            return;
        }
        Err(StreamTaskEventsError::Disconnected) => return,
    }

    if tx
        .send(Ok(Bytes::from_static(b": keepalive\n\n")))
        .await
        .is_err()
    {
        return;
    }
    let mut heartbeat = time::interval_at(
        time::Instant::now() + Duration::from_secs(15),
        Duration::from_secs(15),
    );

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if tx.send(Ok(Bytes::from_static(b": keepalive\n\n"))).await.is_err() {
                    return;
                }
            }
            recv = rx.recv() => {
                match recv {
                    Ok(event) => {
                        if event.id <= last_sent_id || !agent_runtime_task_event_matches_query(&event, &query, &db).await {
                            continue;
                        }
                        last_sent_id = event.id;
                        if tx.send(Ok(Bytes::from(agent_runtime_task_event_sse_frame(&event)))).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        match replay_agent_runtime_task_events(&db, &query, last_sent_id, &tx).await {
                            Ok(next_id) => last_sent_id = next_id,
                            Err(StreamTaskEventsError::Database(err)) => {
                                let frame = agent_runtime_task_events_error_sse_frame(&err.to_string());
                                let _ = tx.send(Ok(Bytes::from(frame))).await;
                                return;
                            }
                            Err(StreamTaskEventsError::Disconnected) => return,
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }
}

enum StreamTaskEventsError {
    Database(sqlx_core::error::Error),
    Disconnected,
}

async fn replay_agent_runtime_task_events(
    db: &PgPool,
    query: &NormalizedAgentRuntimeTaskEventsQuery,
    after_id: i64,
    tx: &mpsc::Sender<Result<Bytes, std::convert::Infallible>>,
) -> Result<i64, StreamTaskEventsError> {
    let mut last_sent_id = after_id;
    let mut replay_query = query.clone();
    loop {
        replay_query.after_id = last_sent_id;
        let events = query_agent_runtime_task_events(db, &replay_query)
            .await
            .map_err(StreamTaskEventsError::Database)?;
        if events.is_empty() {
            return Ok(last_sent_id);
        }
        for event in events {
            last_sent_id = last_sent_id.max(event.id);
            tx.send(Ok(Bytes::from(agent_runtime_task_event_sse_frame(&event))))
                .await
                .map_err(|_| StreamTaskEventsError::Disconnected)?;
        }
    }
}

async fn agent_runtime_task_event_matches_query(
    event: &StoredAgentRuntimeTaskEvent,
    q: &NormalizedAgentRuntimeTaskEventsQuery,
    db: &PgPool,
) -> bool {
    if q.scoped_to_conversation {
        return event.conversation_id == q.conversation_id;
    }
    if event.terminal || is_global_state_task_event_type(&event.event_type) {
        return true;
    }
    agent_runtime_task_is_active(db, &event.conversation_id)
        .await
        .unwrap_or(false)
}

fn is_global_state_task_event_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "task_updated"
            | "task_completed"
            | "task_removed"
            | "hitl_pending_updated"
            | "hitl_decision_updated"
            | "todo_updated"
            | "todo_cleared"
    )
}

async fn agent_runtime_task_is_active(
    db: &PgPool,
    conversation_id: &str,
) -> Result<bool, sqlx_core::error::Error> {
    query_scalar::<_, bool>(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM agent_runtime_tasks
            WHERE conversation_id = $1 AND active = TRUE
        )
        "#,
    )
    .bind(conversation_id)
    .fetch_one(db)
    .await
}

fn agent_runtime_task_event_sse_frame(event: &StoredAgentRuntimeTaskEvent) -> String {
    let mut body = String::new();
    body.push_str("id: ");
    body.push_str(&event.id.to_string());
    body.push('\n');
    body.push_str(&normalize_agent_runtime_task_event_sse_line_for_frontend(
        &event.line,
    ));
    body
}

fn agent_runtime_task_events_error_sse_frame(message: &str) -> String {
    normalize_sse_line(format!(
        "data: {}",
        json!({
            "type": "error",
            "message": message,
            "data": {"source": "rustapi_task_events"}
        })
    ))
}

fn normalize_agent_runtime_task_event_sse_line_for_frontend(line: &str) -> String {
    let Some(envelope) =
        sse_line_json(line).and_then(normalize_command_completed_sse_envelope_for_frontend)
    else {
        return normalize_sse_line(line.to_string());
    };
    normalize_sse_line(format!("data: {envelope}"))
}

fn normalize_command_completed_sse_envelope_for_frontend(mut envelope: Value) -> Option<Value> {
    if !sse_envelope_is_runtime_event(&envelope, "command_completed") {
        return None;
    }
    let obj = envelope.as_object_mut()?;
    obj.insert("type".to_string(), Value::String("done".to_string()));
    let data = obj
        .entry("data".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !data.is_object() {
        *data = Value::Object(serde_json::Map::new());
    }
    if let Some(data_obj) = data.as_object_mut() {
        data_obj
            .entry("runtimeRawEventType".to_string())
            .or_insert_with(|| Value::String("command_completed".to_string()));
        data_obj.insert(
            "runtimeEventType".to_string(),
            Value::String("done".to_string()),
        );
    }
    Some(envelope)
}

fn sse_envelope_is_runtime_event(envelope: &Value, event_type: &str) -> bool {
    let data = envelope.get("data").cloned().unwrap_or(Value::Null);
    let trace = data
        .as_object()
        .and_then(|map| map.get("runtimeTrace"))
        .cloned()
        .unwrap_or(Value::Null);
    [
        json_string_field(envelope, "type"),
        json_string_field(envelope, "event"),
        json_string_field(&data, "runtimeEventType"),
        json_string_field(&data, "runtime_event_type"),
        json_string_field(&trace, "event"),
        json_string_field(&trace, "type"),
    ]
    .into_iter()
    .flatten()
    .any(|value| value == event_type)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedAgentRuntimeStreamRunInput {
    conversation_id: String,
    message: String,
    agent_mode: String,
    assistant_message_id: String,
    user_message_id: String,
    started_at: String,
    created_new: bool,
    background: bool,
    runtime_binary_path: String,
    runtime_work_dir: String,
    runtime_command: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
struct NormalizedAgentRuntimeFrontendTurn {
    conversation_id: String,
    message: String,
    final_message: String,
    project_id: String,
    role: String,
    role_tools: Vec<String>,
    webshell_connection_id: String,
    user_message_id: String,
    assistant_message_id: String,
    started_at: String,
    created_new: bool,
    background: bool,
    runtime_binary_path: String,
    runtime_work_dir: String,
    runtime_command: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeHistoryMessage {
    role: String,
    content: String,
    reasoning_content: String,
}

fn normalize_agent_runtime_stream_input(
    req: AcceptAgentRuntimeStreamRequest,
) -> Result<NormalizedAgentRuntimeStreamRunInput, ApiError> {
    let conversation_id = req.conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Err(ApiError::bad_request("conversationId is required"));
    }
    Ok(NormalizedAgentRuntimeStreamRunInput {
        conversation_id,
        message: req
            .message
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        agent_mode: req
            .agent_mode
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "agent_runtime".to_string()),
        assistant_message_id: req
            .assistant_message_id
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        user_message_id: req
            .user_message_id
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        started_at: req
            .started_at
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        created_new: req.created_new.unwrap_or(false),
        background: req.background.unwrap_or(true),
        runtime_binary_path: req
            .runtime_binary_path
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        runtime_work_dir: req
            .runtime_work_dir
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        runtime_command: req.runtime_command,
    })
}

fn request_has_legacy_runtime_fields(req: &AcceptAgentRuntimeStreamRequest) -> bool {
    req.runtime_command.is_some()
        || req
            .runtime_binary_path
            .as_deref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        || req
            .assistant_message_id
            .as_deref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        || req
            .user_message_id
            .as_deref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
}

async fn prepare_agent_runtime_frontend_turn(
    state: &AppState,
    req: AcceptAgentRuntimeStreamRequest,
) -> Result<NormalizedAgentRuntimeFrontendTurn, ApiError> {
    let message = req
        .message
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    if message.is_empty() {
        return Err(ApiError::bad_request("message is required"));
    }
    let mut project_id = req.project_id.unwrap_or_default().trim().to_string();
    if !project_id.is_empty() {
        ensure_project_exists(&state.db, &project_id).await?;
    }

    let created_new = req.conversation_id.trim().is_empty();
    let conversation_id = if created_new {
        Uuid::new_v4().to_string()
    } else {
        req.conversation_id.trim().to_string()
    };
    if created_new {
        save_conversation(
            &state.db,
            &NormalizedConversationInput {
                id: conversation_id.clone(),
                title: "New Chat".to_string(),
                project_id: project_id.clone(),
                pinned: false,
                created_at: String::new(),
                updated_at: String::new(),
            },
        )
        .await?;
    } else if !conversation_exists(&state.db, &conversation_id).await? {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "对话不存在".to_string(),
        });
    } else if !project_id.is_empty() {
        query("UPDATE conversations SET project_id = $1, updated_at = NOW() WHERE id = $2")
            .bind(&project_id)
            .bind(&conversation_id)
            .execute(&state.db)
            .await?;
    } else {
        project_id = query_conversation_project_id(&state.db, &conversation_id).await?;
    }
    ensure_no_active_agent_runtime_task(&state.db, &conversation_id).await?;

    let history = query_runtime_history_messages(&state.db, &conversation_id).await?;
    let roles = query_roles(&state.db).await?;
    let role_name = req.role.unwrap_or_default().trim().to_string();
    let role = resolve_runtime_role(&roles, &role_name);
    let role_tools = role
        .as_ref()
        .map(|role| role.tools.clone())
        .unwrap_or_default();
    let webshell_connection_id = req
        .webshell_connection_id
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut final_message = apply_runtime_role_prompt(&message, role.as_ref());
    let attachments = req.attachments.unwrap_or_default();
    final_message = append_runtime_attachments(&final_message, &attachments);
    let user_content = user_message_content_for_storage(&message, &attachments);

    let user_message_id = Uuid::new_v4().to_string();
    save_message(
        &state.db,
        &NormalizedMessageInput {
            id: user_message_id.clone(),
            conversation_id: conversation_id.clone(),
            role: "user".to_string(),
            content: user_content.clone(),
            reasoning_content: String::new(),
            mcp_execution_ids: Vec::new(),
            created_at: String::new(),
            updated_at: String::new(),
        },
    )
    .await?;

    let assistant_message_id = Uuid::new_v4().to_string();
    save_message(
        &state.db,
        &NormalizedMessageInput {
            id: assistant_message_id.clone(),
            conversation_id: conversation_id.clone(),
            role: "assistant".to_string(),
            content: "处理中...".to_string(),
            reasoning_content: String::new(),
            mcp_execution_ids: Vec::new(),
            created_at: String::new(),
            updated_at: String::new(),
        },
    )
    .await?;

    if let Some(hitl) = req.hitl {
        save_hitl_config(&state.db, &conversation_id, &hitl).await?;
    }

    if history.is_empty() {
        maybe_spawn_conversation_title_generation(
            state.db.clone(),
            state.task_events.clone(),
            conversation_id.clone(),
            user_content,
        );
    }

    let config = load_or_initialize_config(&state.db).await?;
    let hitl = load_hitl_config(&state.db, &conversation_id).await?;
    let runtime_command = build_agent_runtime_start_turn_command(
        &state.runtime,
        &config,
        &conversation_id,
        &assistant_message_id,
        &final_message,
        &project_id,
        &role_name,
        &webshell_connection_id,
        &role_tools,
        &history,
        req.reasoning.as_ref(),
        &hitl,
    );

    let runtime_binary_path = req
        .runtime_binary_path
        .unwrap_or_default()
        .trim()
        .to_string();
    let runtime_binary_path = if runtime_binary_path.is_empty() {
        state.runtime.binary_path.clone()
    } else {
        runtime_binary_path
    };
    if runtime_binary_path.trim().is_empty() {
        return Err(ApiError::bad_request(
            "AGENT_RUNTIME_BINARY_PATH is required for Rust-owned agent runtime",
        ));
    }
    let runtime_work_dir = req.runtime_work_dir.unwrap_or_default().trim().to_string();
    let runtime_work_dir = if runtime_work_dir.is_empty() {
        state.runtime.work_dir.clone()
    } else {
        runtime_work_dir
    };

    Ok(NormalizedAgentRuntimeFrontendTurn {
        conversation_id,
        message,
        final_message,
        project_id,
        role: role_name,
        role_tools,
        webshell_connection_id,
        user_message_id,
        assistant_message_id,
        started_at: String::new(),
        created_new,
        background: req.background.unwrap_or(true),
        runtime_binary_path,
        runtime_work_dir,
        runtime_command,
    })
}

fn frontend_turn_to_stream_run(
    turn: &NormalizedAgentRuntimeFrontendTurn,
) -> NormalizedAgentRuntimeStreamRunInput {
    NormalizedAgentRuntimeStreamRunInput {
        conversation_id: turn.conversation_id.clone(),
        message: turn.message.clone(),
        agent_mode: "agent_runtime".to_string(),
        assistant_message_id: turn.assistant_message_id.clone(),
        user_message_id: turn.user_message_id.clone(),
        started_at: turn.started_at.clone(),
        created_new: turn.created_new,
        background: turn.background,
        runtime_binary_path: turn.runtime_binary_path.clone(),
        runtime_work_dir: turn.runtime_work_dir.clone(),
        runtime_command: Some(turn.runtime_command.clone()),
    }
}

async fn ensure_no_active_agent_runtime_task(
    db: &PgPool,
    conversation_id: &str,
) -> Result<(), ApiError> {
    let active = query_scalar::<_, bool>(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM agent_runtime_tasks
            WHERE conversation_id = $1 AND active = TRUE
        )
        "#,
    )
    .bind(conversation_id)
    .fetch_one(db)
    .await?;
    if active {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: "当前会话已有任务正在执行中".to_string(),
        });
    }
    Ok(())
}

async fn query_runtime_history_messages(
    db: &PgPool,
    conversation_id: &str,
) -> Result<Vec<RuntimeHistoryMessage>, sqlx_core::error::Error> {
    let rows = query(
        r#"
        SELECT role, content, COALESCE(reasoning_content, '') AS reasoning_content
        FROM messages
        WHERE conversation_id = $1
        ORDER BY created_at ASC, id ASC
        "#,
    )
    .bind(conversation_id)
    .fetch_all(db)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(RuntimeHistoryMessage {
            role: row.try_get("role")?,
            content: row.try_get("content")?,
            reasoning_content: row.try_get("reasoning_content")?,
        });
    }
    Ok(out)
}

fn resolve_runtime_role(roles: &[RoleItem], role_name: &str) -> Option<RoleItem> {
    let role_name = role_name.trim();
    if role_name.is_empty() || role_name == "默认" {
        return None;
    }
    roles
        .iter()
        .find(|role| role.enabled && role.name == role_name)
        .cloned()
}

fn apply_runtime_role_prompt(message: &str, role: Option<&RoleItem>) -> String {
    let Some(role) = role else {
        return message.to_string();
    };
    let prompt = role.user_prompt.trim();
    if prompt.is_empty() {
        message.to_string()
    } else {
        format!("{prompt}\n\n{message}")
    }
}

fn append_runtime_attachments(message: &str, attachments: &[ChatAttachment]) -> String {
    if attachments.is_empty() {
        return message.to_string();
    }
    let mut out = message.trim().to_string();
    out.push_str("\n\nAttachments:");
    for attachment in attachments {
        let name = first_non_empty([
            Some(attachment.file_name.as_str()),
            Some(attachment.server_path.as_str()),
            Some(attachment.mime_type.as_str()),
        ]);
        if name.is_empty() {
            continue;
        }
        out.push_str("\n- ");
        out.push_str(&name);
        if !attachment.server_path.trim().is_empty() {
            out.push_str(" (");
            out.push_str(attachment.server_path.trim());
            out.push(')');
        }
    }
    out
}

fn user_message_content_for_storage(message: &str, attachments: &[ChatAttachment]) -> String {
    append_runtime_attachments(message, attachments)
}

fn build_agent_runtime_start_turn_command(
    runtime: &RuntimeSettings,
    config: &Value,
    conversation_id: &str,
    assistant_message_id: &str,
    message: &str,
    project_id: &str,
    role: &str,
    webshell_connection_id: &str,
    role_tools: &[String],
    history: &[RuntimeHistoryMessage],
    reasoning: Option<&Value>,
    hitl: &HitlConfig,
) -> Value {
    let openai = config.get("openai").and_then(Value::as_object);
    let mut context = Map::new();
    insert_json_string(&mut context, "role", role);
    insert_json_string(&mut context, "project_id", project_id);
    insert_json_string(
        &mut context,
        "webshell_connection_id",
        webshell_connection_id,
    );
    insert_json_string(
        &mut context,
        "openai_provider",
        openai
            .and_then(|v| v.get("provider"))
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    insert_json_string(
        &mut context,
        "openai_api_key",
        openai
            .and_then(|v| v.get("api_key"))
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    insert_json_string(
        &mut context,
        "openai_base_url",
        openai
            .and_then(|v| v.get("base_url"))
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    insert_json_string(
        &mut context,
        "openai_model",
        openai
            .and_then(|v| v.get("model"))
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let reasoning_effort = reasoning
        .and_then(|value| value.get("effort"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            openai
                .and_then(|v| v.get("reasoning"))
                .and_then(|v| v.get("effort"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_default();
    insert_json_string(&mut context, "openai_reasoning_effort", reasoning_effort);
    context.insert("max_steps".to_string(), json!(runtime.max_steps));
    context.insert(
        "tool_timeout_seconds".to_string(),
        json!(runtime.tool_timeout_seconds),
    );
    insert_json_string(&mut context, "workspace_root", &runtime.work_dir);
    context.insert(
        "filesystem_enabled".to_string(),
        json!(!runtime.work_dir.trim().is_empty()),
    );
    if !runtime.work_dir.trim().is_empty() {
        insert_json_string(
            &mut context,
            "session_store_dir",
            &format!(
                "{}/.cyberstrike-agent-runtime/sessions",
                runtime.work_dir.trim_end_matches('/')
            ),
        );
    }
    context.insert(
        "conversation_history".to_string(),
        json!(runtime_history_context(history)),
    );
    let mcp_tools_dir = if runtime.work_dir.trim().is_empty() {
        String::new()
    } else {
        format!("{}/tools", runtime.work_dir.trim_end_matches('/'))
    };
    context.insert(
        "mcp_enabled".to_string(),
        json!(!mcp_tools_dir.trim().is_empty() || !runtime.mcp_endpoint_url.is_empty()),
    );
    insert_json_string(&mut context, "mcp_endpoint_url", &runtime.mcp_endpoint_url);
    insert_json_string(
        &mut context,
        "external_mcp_endpoint_url",
        &runtime.mcp_endpoint_url,
    );
    insert_json_string(&mut context, "mcp_auth_header", &runtime.mcp_auth_header);
    insert_json_string(
        &mut context,
        "mcp_auth_header_value",
        &runtime.mcp_auth_header_value,
    );
    context.insert("role_tools".to_string(), json!(role_tools));
    if !mcp_tools_dir.trim().is_empty() {
        insert_json_string(&mut context, "mcp_tools_dir", &mcp_tools_dir);
    }
    context.insert("mcp_tools".to_string(), json!([]));
    context.insert(
        "skills_enabled".to_string(),
        json!(!runtime.skills_dir.trim().is_empty()),
    );
    insert_json_string(&mut context, "skills_dir", &runtime.skills_dir);
    context.insert("skills_source".to_string(), json!("rust_dir"));
    context.insert(
        "skills_allowlist".to_string(),
        json!(runtime_skill_allowlist(role_tools)),
    );
    context.insert("knowledge_enabled".to_string(), json!(false));
    context.insert("knowledge_snippets".to_string(), json!([]));
    let approval_enabled = hitl_config_effective(hitl);
    context.insert("approval_enabled".to_string(), json!(approval_enabled));
    context.insert(
        "approval_allowlist".to_string(),
        json!(hitl.sensitive_tools.clone()),
    );
    if hitl.timeout_seconds > 0 {
        context.insert(
            "hitl_timeout_seconds".to_string(),
            json!(hitl.timeout_seconds),
        );
        context.insert(
            "approval_timeout_seconds".to_string(),
            json!(hitl.timeout_seconds),
        );
    }
    context.insert("compaction_enabled".to_string(), json!(false));
    context.insert(
        "rust_owned_runtime_todos".to_string(),
        json!(["knowledge_snippets"]),
    );
    insert_json_string(&mut context, "assistant_message_id", assistant_message_id);
    insert_json_string(&mut context, "assistantMessageId", assistant_message_id);

    json!({
        "type": "start_turn",
        "conversation_id": conversation_id,
        "runtime_session_id": null,
        "message": message,
        "context": Value::Object(context),
    })
}

fn insert_json_string(context: &mut Map<String, Value>, key: &str, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        context.insert(key.to_string(), Value::String(value.to_string()));
    }
}

fn runtime_history_context(history: &[RuntimeHistoryMessage]) -> Vec<Value> {
    const MAX_HISTORY_MESSAGES: usize = 24;
    let start = history.len().saturating_sub(MAX_HISTORY_MESSAGES);
    history[start..]
        .iter()
        .filter_map(|msg| {
            let role = msg.role.trim().to_ascii_lowercase();
            if !matches!(role.as_str(), "user" | "assistant" | "system" | "tool") {
                return None;
            }
            let content = msg.content.trim();
            if content.is_empty() || content == "处理中..." {
                return None;
            }
            let mut item = Map::new();
            item.insert("role".to_string(), Value::String(role));
            item.insert(
                "content".to_string(),
                Value::String(truncate_chars(content, 4000)),
            );
            if !msg.reasoning_content.trim().is_empty() {
                item.insert(
                    "reasoning_content".to_string(),
                    Value::String(truncate_chars(&msg.reasoning_content, 2000)),
                );
            }
            Some(Value::Object(item))
        })
        .collect()
}

fn runtime_skill_allowlist(role_tools: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for raw in role_tools {
        let mut value = raw.trim();
        let lower = value.to_ascii_lowercase();
        if lower.starts_with("skill::") {
            value = value[7..].trim();
        } else if lower.starts_with("skill:") {
            value = value[6..].trim();
        } else {
            continue;
        }
        if !value.is_empty() && !out.iter().any(|item| item == value) {
            out.push(value.to_string());
        }
    }
    out
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect()
}

fn maybe_spawn_conversation_title_generation(
    db: PgPool,
    bus: AgentRuntimeTaskEventBus,
    conversation_id: String,
    first_user_message: String,
) {
    tokio::spawn(async move {
        let _ = generate_and_publish_conversation_title(
            &db,
            &bus,
            &conversation_id,
            &first_user_message,
        )
        .await;
    });
}

async fn generate_and_publish_conversation_title(
    db: &PgPool,
    bus: &AgentRuntimeTaskEventBus,
    conversation_id: &str,
    first_user_message: &str,
) -> Result<(), sqlx_core::error::Error> {
    let current = query_scalar::<_, String>("SELECT title FROM conversations WHERE id = $1")
        .bind(conversation_id)
        .fetch_optional(db)
        .await?
        .unwrap_or_default();
    if !is_default_conversation_title(&current) {
        return Ok(());
    }
    let title = fallback_conversation_title(first_user_message);
    let updated = query(
        r#"
        UPDATE conversations
        SET title = $1, updated_at = NOW()
        WHERE id = $2 AND title IN ('New Chat', '新对话', '')
        "#,
    )
    .bind(&title)
    .bind(conversation_id)
    .execute(db)
    .await?
    .rows_affected();
    if updated == 0 {
        return Ok(());
    }
    let event = conversation_title_updated_event(conversation_id, &title);
    let _ = save_and_publish_state_event(db, bus, event).await?;
    Ok(())
}

fn is_default_conversation_title(title: &str) -> bool {
    matches!(title.trim(), "" | "New Chat" | "新对话")
}

fn fallback_conversation_title(message: &str) -> String {
    let mut title = message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    if title.is_empty() {
        title = "New Chat".to_string();
    }
    let truncated = truncate_chars(&title, 40);
    if truncated.chars().count() < title.chars().count() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn conversation_title_updated_event(
    conversation_id: &str,
    title: &str,
) -> NormalizedAgentRuntimeTaskEventInput {
    let conversation_id = conversation_id.trim().to_string();
    NormalizedAgentRuntimeTaskEventInput {
        conversation_id: conversation_id.clone(),
        line: normalize_sse_line(format!(
            "data: {}",
            json!({
                "type": "conversation_title_updated",
                "message": title,
                "data": {
                    "conversationId": conversation_id,
                    "title": title,
                }
            })
        )),
        runtime_event_id: String::new(),
        event_type: "conversation_title_updated".to_string(),
        terminal: false,
    }
}

async fn save_agent_runtime_stream_run(
    db: &PgPool,
    run: &NormalizedAgentRuntimeStreamRunInput,
) -> Result<(), sqlx_core::error::Error> {
    query(
        r#"
        INSERT INTO agent_runtime_stream_runs
            (conversation_id, message, agent_mode, assistant_message_id, user_message_id, background, created_new, status, started_at, created_at, updated_at)
        VALUES
            ($1, $2, $3, NULLIF($4, ''), NULLIF($5, ''), $6, $7, 'accepted', COALESCE(NULLIF($8, '')::timestamptz, NOW()), NOW(), NOW())
        "#,
    )
    .bind(&run.conversation_id)
    .bind(&run.message)
    .bind(&run.agent_mode)
    .bind(&run.assistant_message_id)
    .bind(&run.user_message_id)
    .bind(run.background)
    .bind(run.created_new)
    .bind(&run.started_at)
    .execute(db)
    .await?;
    Ok(())
}

fn spawn_agent_runtime_jsonl_run(
    internal_base_url: String,
    db: PgPool,
    processes: RuntimeProcessRegistry,
    bus: AgentRuntimeTaskEventBus,
    run: NormalizedAgentRuntimeStreamRunInput,
) {
    if run.runtime_command.is_none() || run.runtime_binary_path.trim().is_empty() {
        return;
    }
    tokio::spawn(async move {
        let failed_task = NormalizedAgentRuntimeTaskInput {
            conversation_id: run.conversation_id.clone(),
            message: run.message.clone(),
            status: "failed".to_string(),
            agent_mode: run.agent_mode.clone(),
            assistant_message_id: run.assistant_message_id.clone(),
            started_at: run.started_at.clone(),
            active: false,
        };
        if let Err(err) = run_agent_runtime_jsonl_background(
            internal_base_url,
            db.clone(),
            processes,
            bus.clone(),
            run,
        )
        .await
        {
            let conversation_id = err.0;
            let message = err.1;
            let _ = save_and_publish_agent_runtime_task_event(
                &db,
                &bus,
                &NormalizedAgentRuntimeTaskEventInput {
                    conversation_id: conversation_id.clone(),
                    line: normalize_sse_line(format!(
                        "data: {}",
                        json!({
                            "type": "error",
                            "message": message,
                            "data": {"conversationId": conversation_id}
                        })
                    )),
                    runtime_event_id: String::new(),
                    event_type: "error".to_string(),
                    terminal: true,
                },
            )
            .await;
            let _ = finalize_agent_runtime_assistant_message(
                &db,
                &conversation_id,
                &failed_task.assistant_message_id,
                &message,
                "",
            )
            .await;
            let _ = save_and_publish_agent_runtime_task_state(&db, &bus, &failed_task).await;
        }
    });
}

async fn run_agent_runtime_jsonl_background(
    internal_base_url: String,
    db: PgPool,
    processes: RuntimeProcessRegistry,
    bus: AgentRuntimeTaskEventBus,
    run: NormalizedAgentRuntimeStreamRunInput,
) -> Result<(), (String, String)> {
    let conversation_id = run.conversation_id.clone();
    let mut command = run.runtime_command.clone().ok_or_else(|| {
        (
            conversation_id.clone(),
            "runtimeCommand is required".to_string(),
        )
    })?;
    inject_permission_context(&mut command, &internal_base_url, &run.assistant_message_id);
    let mut child = TokioCommand::new(&run.runtime_binary_path);
    if !run.runtime_work_dir.trim().is_empty() {
        child.current_dir(PathBuf::from(run.runtime_work_dir.trim()));
    }
    child
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = child.spawn().map_err(|err| {
        (
            conversation_id.clone(),
            format!("start agent runtime: {err}"),
        )
    })?;
    let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
    processes
        .lock()
        .await
        .insert(conversation_id.clone(), cancel_tx);
    if let Some(mut stdin) = child.stdin.take() {
        let raw = serde_json::to_vec(&command).map_err(|err| {
            (
                conversation_id.clone(),
                format!("serialize runtime command: {err}"),
            )
        })?;
        stdin.write_all(&raw).await.map_err(|err| {
            (
                conversation_id.clone(),
                format!("write runtime command: {err}"),
            )
        })?;
        stdin.write_all(b"\n").await.map_err(|err| {
            (
                conversation_id.clone(),
                format!("write runtime command newline: {err}"),
            )
        })?;
    }
    drop(child.stdin.take());

    let stdout = child.stdout.take();
    let read_events = async {
        let mut saw_terminal_event = false;
        if let Some(stdout) = stdout {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next_line().await.map_err(|err| {
                (
                    conversation_id.clone(),
                    format!("read runtime event: {err}"),
                )
            })? {
                if line.trim().is_empty() {
                    continue;
                }
                let (event_type, runtime_event_id, terminal) =
                    runtime_event_identity_from_json(&line);
                if terminal {
                    saw_terminal_event = true;
                }
                let sse_line = runtime_event_json_to_sse_line(&conversation_id, &line);
                save_and_publish_agent_runtime_task_event(
                    &db,
                    &bus,
                    &NormalizedAgentRuntimeTaskEventInput {
                        conversation_id: conversation_id.clone(),
                        line: sse_line,
                        runtime_event_id,
                        event_type,
                        terminal,
                    },
                )
                .await
                .map_err(|err| {
                    (
                        conversation_id.clone(),
                        format!("persist runtime event: {err}"),
                    )
                })?;
            }
        }
        Ok::<bool, (String, String)>(saw_terminal_event)
    };

    let saw_terminal_event = tokio::select! {
        read_result = read_events => {
            read_result?
        }
        _ = &mut cancel_rx => {
            let _ = child.kill().await;
            processes.lock().await.remove(&conversation_id);
            let cancellation_event = agent_runtime_cancellation_task_event(
                &conversation_id,
                "Agent Runtime 已取消",
                "cancelled",
            );
            save_and_publish_agent_runtime_task_event(&db, &bus, &cancellation_event)
                .await
                .map_err(|err| {
                    (
                        conversation_id.clone(),
                        format!("persist cancellation event: {err}"),
                    )
                })?;
            finalize_agent_runtime_assistant_message(
                &db,
                &conversation_id,
                &run.assistant_message_id,
                "Agent Runtime 已取消",
                "",
            )
            .await
            .map_err(|err| {
                (
                    conversation_id.clone(),
                    format!("finalize cancelled assistant message: {err}"),
                )
            })?;
            let cancelled = NormalizedAgentRuntimeTaskInput {
                conversation_id: run.conversation_id,
                message: run.message,
                status: "cancelled".to_string(),
                agent_mode: run.agent_mode,
                assistant_message_id: run.assistant_message_id,
                started_at: run.started_at,
                active: false,
            };
            save_and_publish_agent_runtime_task_state(&db, &bus, &cancelled)
                .await
                .map_err(|err| (cancelled.conversation_id.clone(), format!("cancel task: {err}")))?;
            return Ok(());
        }
    };

    let status = child.wait().await.map_err(|err| {
        (
            conversation_id.clone(),
            format!("wait agent runtime: {err}"),
        )
    })?;
    processes.lock().await.remove(&conversation_id);
    if !status.success() {
        return Err((conversation_id, format!("agent runtime exited: {status}")));
    }
    if !saw_terminal_event {
        let completed_event = agent_runtime_completed_task_event(&conversation_id);
        save_and_publish_agent_runtime_task_event(&db, &bus, &completed_event)
            .await
            .map_err(|err| {
                (
                    completed_event.conversation_id.clone(),
                    format!("persist completion event: {err}"),
                )
            })?;
    }
    let (final_response, final_reasoning) =
        query_agent_runtime_final_response_and_reasoning(&db, &conversation_id)
            .await
            .map_err(|err| {
                (
                    conversation_id.clone(),
                    format!("query final assistant response: {err}"),
                )
            })?;
    finalize_agent_runtime_assistant_message(
        &db,
        &conversation_id,
        &run.assistant_message_id,
        &final_response,
        &final_reasoning,
    )
    .await
    .map_err(|err| {
        (
            conversation_id.clone(),
            format!("finalize assistant message: {err}"),
        )
    })?;
    let final_task = NormalizedAgentRuntimeTaskInput {
        conversation_id: run.conversation_id,
        message: run.message,
        status: "completed".to_string(),
        agent_mode: run.agent_mode,
        assistant_message_id: run.assistant_message_id,
        started_at: run.started_at,
        active: false,
    };
    save_and_publish_agent_runtime_task_state(&db, &bus, &final_task)
        .await
        .map_err(|err| {
            (
                final_task.conversation_id.clone(),
                format!("finish task: {err}"),
            )
        })?;
    Ok(())
}

fn agent_runtime_completed_task_event(
    conversation_id: &str,
) -> NormalizedAgentRuntimeTaskEventInput {
    let conversation_id = conversation_id.trim().to_string();
    NormalizedAgentRuntimeTaskEventInput {
        conversation_id: conversation_id.clone(),
        line: normalize_sse_line(format!(
            "data: {}",
            json!({
                "type": "done",
                "message": "",
                "data": {
                    "conversationId": conversation_id,
                    "runtimeEventType": "done",
                    "runtimeTrace": {
                        "type": "command_completed",
                    },
                }
            })
        )),
        runtime_event_id: String::new(),
        event_type: "command_completed".to_string(),
        terminal: true,
    }
}

fn agent_runtime_cancellation_task_event(
    conversation_id: &str,
    message: &str,
    reason: &str,
) -> NormalizedAgentRuntimeTaskEventInput {
    let conversation_id = conversation_id.trim().to_string();
    let reason = reason.trim();
    NormalizedAgentRuntimeTaskEventInput {
        conversation_id: conversation_id.clone(),
        line: normalize_sse_line(format!(
            "data: {}",
            json!({
                "type": "cancelled",
                "message": message,
                "data": {
                    "conversationId": conversation_id,
                    "runtimeEventType": "turn_aborted",
                    "runtimeTrace": {
                        "type": "turn_aborted",
                        "reason": reason,
                    },
                }
            })
        )),
        runtime_event_id: String::new(),
        event_type: "turn_aborted".to_string(),
        terminal: true,
    }
}

fn runtime_event_identity_from_json(raw: &str) -> (String, String, bool) {
    let value: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("progress")
        .trim()
        .to_string();
    let runtime_event_id = value
        .get("event_id")
        .or_else(|| value.get("runtime_event_id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let terminal = matches!(
        event_type.as_str(),
        "turn_completed" | "turn_aborted" | "runtime_error" | "command_completed"
    );
    (event_type, runtime_event_id, terminal)
}

fn runtime_event_json_to_sse_line(conversation_id: &str, raw: &str) -> String {
    let value: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("progress")
        .to_string();
    let message = value
        .get("message")
        .or_else(|| value.get("delta"))
        .or_else(|| value.get("response"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let frontend_type = runtime_event_to_frontend_type(&event_type);
    let frontend_runtime_event_type = runtime_event_type_for_frontend(&event_type);
    let mut data = serde_json::Map::new();
    data.insert(
        "conversationId".to_string(),
        Value::String(conversation_id.to_string()),
    );
    data.insert(
        "runtimeEventType".to_string(),
        Value::String(frontend_runtime_event_type.to_string()),
    );
    if frontend_runtime_event_type != event_type {
        data.insert(
            "runtimeRawEventType".to_string(),
            Value::String(event_type.clone()),
        );
    }
    data.insert(
        "runtimeEventId".to_string(),
        value
            .get("event_id")
            .or_else(|| value.get("runtime_event_id"))
            .cloned()
            .unwrap_or(Value::Null),
    );
    data.insert("runtimeTrace".to_string(), value);
    normalize_sse_line(format!(
        "data: {}",
        json!({
            "type": frontend_type,
            "message": message,
            "data": Value::Object(data)
        })
    ))
}

fn runtime_event_type_for_frontend(event_type: &str) -> &str {
    match event_type {
        "command_completed" => "done",
        _ => event_type,
    }
}

fn runtime_event_to_frontend_type(event_type: &str) -> &'static str {
    match event_type {
        "assistant_delta" => "response_delta",
        "reasoning_delta" => "reasoning_chain_stream_delta",
        "plan_updated" => "planning",
        "tool_call_started" => "tool_call",
        "tool_call_delta" => "tool_result_delta",
        "tool_call_completed" | "tool_call_failed" => "tool_result",
        "approval_requested" => "hitl_approval_requested",
        "approval_resolved" => "hitl_approval_resolved",
        "turn_aborted" => "cancelled",
        "runtime_error" => "error",
        "turn_completed" => "turn_completed",
        "command_completed" => "done",
        "assistant_progress_update" => "assistant_progress_update",
        "runtime_status_update" => "runtime_status_update",
        _ => "progress",
    }
}

fn agent_runtime_stream_accepted_sse_response(
    run: &NormalizedAgentRuntimeStreamRunInput,
) -> Response {
    let mut frames = Vec::new();
    if run.created_new {
        frames.push(json!({
            "type": "conversation",
            "message": "会话已创建",
            "data": {
                "conversationId": run.conversation_id,
                "background": run.background,
            }
        }));
    }
    if !run.user_message_id.trim().is_empty() {
        frames.push(json!({
            "type": "message_saved",
            "message": "",
            "data": {
                "conversationId": run.conversation_id,
                "userMessageId": run.user_message_id,
                "assistantMessageId": run.assistant_message_id,
                "background": run.background,
            }
        }));
    }
    frames.push(json!({
        "type": "runtime_status_update",
        "message": "Agent Runtime 已在后台启动",
        "data": {
            "conversationId": run.conversation_id,
            "runtimeEventType": "runtime_status_update",
            "background": run.background,
            "agentMode": run.agent_mode,
            "userMessageId": run.user_message_id,
            "assistantMessageId": run.assistant_message_id,
        }
    }));
    frames.push(json!({
        "type": "done",
        "message": "",
        "data": {
            "conversationId": run.conversation_id,
            "userMessageId": run.user_message_id,
            "assistantMessageId": run.assistant_message_id,
            "background": run.background,
        }
    }));
    let body = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>();
    Response::builder()
        .status(StatusCode::OK)
        .header(
            "content-type",
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        )
        .header("cache-control", HeaderValue::from_static("no-cache"))
        .header("connection", HeaderValue::from_static("keep-alive"))
        .header("x-accel-buffering", HeaderValue::from_static("no"))
        .body(Body::from(body))
        .expect("valid SSE response")
}

#[derive(Debug, PartialEq, Eq)]
struct NormalizedAgentRuntimeTaskInput {
    conversation_id: String,
    message: String,
    status: String,
    agent_mode: String,
    assistant_message_id: String,
    started_at: String,
    active: bool,
}

fn normalize_agent_runtime_task_input(
    conversation_id: String,
    req: UpsertAgentRuntimeTaskRequest,
) -> Result<NormalizedAgentRuntimeTaskInput, ApiError> {
    let conversation_id = conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Err(ApiError::bad_request("conversationId is required"));
    }
    let status = req
        .status
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "running".to_string());
    let agent_mode = req
        .agent_mode
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "agent_runtime".to_string());
    Ok(NormalizedAgentRuntimeTaskInput {
        conversation_id,
        message: req
            .message
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        status,
        agent_mode,
        assistant_message_id: req
            .assistant_message_id
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        started_at: req
            .started_at
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        active: req.active.unwrap_or(true),
    })
}

async fn save_agent_runtime_task(
    db: &PgPool,
    task: &NormalizedAgentRuntimeTaskInput,
) -> Result<(), sqlx_core::error::Error> {
    query(
        r#"
        INSERT INTO agent_runtime_tasks
            (conversation_id, message, status, agent_mode, assistant_message_id, started_at, updated_at, finished_at, active)
        VALUES
            ($1, $2, $3, $4, NULLIF($5, ''), COALESCE(NULLIF($6, '')::timestamptz, NOW()), NOW(), CASE WHEN $7 THEN NULL ELSE NOW() END, $7)
        ON CONFLICT (conversation_id) DO UPDATE SET
            message = CASE WHEN EXCLUDED.message = '' THEN agent_runtime_tasks.message ELSE EXCLUDED.message END,
            status = EXCLUDED.status,
            agent_mode = CASE WHEN EXCLUDED.agent_mode = '' THEN agent_runtime_tasks.agent_mode ELSE EXCLUDED.agent_mode END,
            assistant_message_id = COALESCE(EXCLUDED.assistant_message_id, agent_runtime_tasks.assistant_message_id),
            started_at = COALESCE(NULLIF($6, '')::timestamptz, agent_runtime_tasks.started_at),
            updated_at = NOW(),
            finished_at = CASE WHEN EXCLUDED.active THEN NULL ELSE COALESCE(agent_runtime_tasks.finished_at, NOW()) END,
            active = EXCLUDED.active
        "#,
    )
    .bind(&task.conversation_id)
    .bind(&task.message)
    .bind(&task.status)
    .bind(&task.agent_mode)
    .bind(&task.assistant_message_id)
    .bind(&task.started_at)
    .bind(task.active)
    .execute(db)
    .await?;
    Ok(())
}

async fn save_and_publish_agent_runtime_task_state(
    db: &PgPool,
    bus: &AgentRuntimeTaskEventBus,
    task: &NormalizedAgentRuntimeTaskInput,
) -> Result<(), sqlx_core::error::Error> {
    save_agent_runtime_task(db, task).await?;
    if let Some(snapshot) = query_agent_runtime_task(db, &task.conversation_id).await? {
        let event = agent_runtime_task_state_event(&task.conversation_id, snapshot, !task.active);
        let _ = save_and_publish_state_event(db, bus, event).await?;
    }
    Ok(())
}

fn agent_runtime_task_state_event(
    conversation_id: &str,
    task: Value,
    completed: bool,
) -> NormalizedAgentRuntimeTaskEventInput {
    let conversation_id = conversation_id.trim().to_string();
    let event_type = if completed {
        "task_completed"
    } else {
        "task_updated"
    };
    let status = task
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    NormalizedAgentRuntimeTaskEventInput {
        conversation_id: conversation_id.clone(),
        line: normalize_sse_line(format!(
            "data: {}",
            json!({
                "type": event_type,
                "message": status,
                "data": {
                    "conversationId": conversation_id,
                    "task": task,
                }
            })
        )),
        runtime_event_id: String::new(),
        event_type: event_type.to_string(),
        terminal: completed,
    }
}

async fn cancel_agent_runtime_task_in_db(
    db: &PgPool,
    bus: &AgentRuntimeTaskEventBus,
    req: CancelAgentRuntimeTaskRequest,
) -> Result<CancelAgentRuntimeTaskResponse, ApiError> {
    if req.continue_after.unwrap_or(false) {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: "Agent Runtime 暂不支持中断并继续，请停止当前任务后重新发送补充内容。"
                .to_string(),
        });
    }
    let conversation_id = req.conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Err(ApiError::bad_request("conversationId is required"));
    }
    let rows = query(
        r#"
        UPDATE agent_runtime_tasks
        SET status = 'cancelling',
            active = TRUE,
            updated_at = NOW(),
            finished_at = NULL
        WHERE conversation_id = $1
          AND active = TRUE
        "#,
    )
    .bind(&conversation_id)
    .execute(db)
    .await?
    .rows_affected();
    if rows == 0 {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "未找到正在执行的任务".to_string(),
        });
    }
    if let Some(snapshot) = query_agent_runtime_task(db, &conversation_id).await? {
        let event = agent_runtime_task_state_event(&conversation_id, snapshot, false);
        let _ = save_and_publish_state_event(db, bus, event).await?;
    }
    Ok(cancel_agent_runtime_response(
        conversation_id,
        req.reason.unwrap_or_default(),
    ))
}

fn cancel_agent_runtime_response(
    conversation_id: String,
    reason: String,
) -> CancelAgentRuntimeTaskResponse {
    CancelAgentRuntimeTaskResponse {
        status: "cancelling".to_string(),
        conversation_id,
        message: "已提交取消请求，任务将在当前步骤完成后停止。".to_string(),
        continue_after: false,
        interrupt_with_note: !reason.trim().is_empty(),
        agent_mode: "agent_runtime".to_string(),
    }
}

async fn load_hitl_config(
    db: &PgPool,
    conversation_id: &str,
) -> Result<HitlConfig, sqlx_core::error::Error> {
    let row = query(
        r#"
        SELECT enabled, mode, sensitive_tools, timeout_seconds::bigint AS timeout_seconds
        FROM hitl_conversation_configs
        WHERE conversation_id = $1
        "#,
    )
    .bind(conversation_id)
    .fetch_optional(db)
    .await?;
    let Some(row) = row else {
        return Ok(default_hitl_config());
    };
    let enabled: bool = row.try_get("enabled")?;
    let mode: String = row.try_get("mode")?;
    let sensitive_tools: Value = row.try_get("sensitive_tools")?;
    let timeout_seconds: i64 = row.try_get("timeout_seconds")?;
    Ok(normalize_hitl_config(HitlConfig {
        enabled,
        mode,
        sensitive_tools: json_string_array(sensitive_tools),
        timeout_seconds,
    }))
}

async fn save_hitl_config(
    db: &PgPool,
    conversation_id: &str,
    hitl: &HitlConfig,
) -> Result<(), sqlx_core::error::Error> {
    let hitl = normalize_hitl_config(hitl.clone());
    let sensitive_tools = json!(hitl.sensitive_tools);
    query(
        r#"
        INSERT INTO hitl_conversation_configs
            (conversation_id, enabled, mode, sensitive_tools, timeout_seconds, updated_at)
        VALUES ($1, $2, $3, $4, $5, NOW())
        ON CONFLICT (conversation_id)
        DO UPDATE SET enabled = EXCLUDED.enabled,
                      mode = EXCLUDED.mode,
                      sensitive_tools = EXCLUDED.sensitive_tools,
                      timeout_seconds = EXCLUDED.timeout_seconds,
                      updated_at = NOW()
        "#,
    )
    .bind(conversation_id)
    .bind(hitl.enabled)
    .bind(&hitl.mode)
    .bind(&sensitive_tools)
    .bind(hitl.timeout_seconds)
    .execute(db)
    .await?;
    Ok(())
}

async fn latest_pending_hitl_mode(
    db: &PgPool,
    conversation_id: &str,
) -> Result<Option<String>, sqlx_core::error::Error> {
    query_scalar::<_, String>(
        r#"
        SELECT mode
        FROM hitl_interrupts
        WHERE conversation_id = $1 AND status = 'pending'
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(conversation_id)
    .fetch_optional(db)
    .await
}

async fn query_hitl_pending(
    db: &PgPool,
    q: &NormalizedHitlPendingQuery,
) -> Result<Vec<HitlPendingItem>, sqlx_core::error::Error> {
    let rows = query(
        r#"
        SELECT id, conversation_id, COALESCE(message_id, '') AS message_id,
               mode, tool_name, COALESCE(tool_call_id, '') AS tool_call_id,
               COALESCE(payload, '') AS payload, status,
               COALESCE(decision, '') AS decision,
               COALESCE(decision_comment, '') AS decision_comment,
               created_at::text AS created_at,
               decided_at::text AS decided_at
        FROM hitl_interrupts
        WHERE ($1 = '' OR conversation_id = $1)
          AND ($2 = TRUE OR status = $3)
        ORDER BY created_at DESC
        LIMIT $4 OFFSET $5
        "#,
    )
    .bind(&q.conversation_id)
    .bind(q.status_all)
    .bind(&q.status)
    .bind(q.page_size)
    .bind(q.offset())
    .fetch_all(db)
    .await?;

    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let tool_name: String = row.try_get("tool_name")?;
        let payload: String = row.try_get("payload")?;
        let payload_value = parse_payload_json(&payload);
        let (permission, patterns, always, metadata) =
            hitl_permission_fields(&payload_value, tool_name.clone());
        items.push(HitlPendingItem {
            id: row.try_get("id")?,
            conversation_id: row.try_get("conversation_id")?,
            message_id: row.try_get("message_id")?,
            mode: row.try_get("mode")?,
            tool_name,
            tool_call_id: row.try_get("tool_call_id")?,
            payload,
            permission,
            patterns,
            always,
            metadata,
            status: row.try_get("status")?,
            decision: row.try_get("decision")?,
            comment: row.try_get("decision_comment")?,
            created_at: row.try_get("created_at")?,
            decided_at: row.try_get("decided_at")?,
        });
    }
    Ok(items)
}

async fn query_hitl_interrupt_conversation_id(
    db: &PgPool,
    interrupt_id: &str,
) -> Result<Option<String>, sqlx_core::error::Error> {
    query_scalar::<_, String>(
        r#"
        SELECT conversation_id
        FROM hitl_interrupts
        WHERE id = $1
        "#,
    )
    .bind(interrupt_id)
    .fetch_optional(db)
    .await
}

async fn query_hitl_event_fields(
    db: &PgPool,
    interrupt_id: &str,
) -> Result<Option<HitlEventFields>, sqlx_core::error::Error> {
    let row = query(
        r#"
        SELECT id, tool_name, COALESCE(tool_call_id, '') AS tool_call_id,
               status, COALESCE(payload, '') AS payload
        FROM hitl_interrupts
        WHERE id = $1
        "#,
    )
    .bind(interrupt_id)
    .fetch_optional(db)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let interrupt_id: String = row.try_get("id")?;
    let tool_name: String = row.try_get("tool_name")?;
    let tool_call_id: String = row.try_get("tool_call_id")?;
    let status: String = row.try_get("status")?;
    let payload: String = row.try_get("payload")?;
    let payload_value = parse_payload_json(&payload);
    let (permission, patterns, always, metadata) =
        hitl_permission_fields(&payload_value, tool_name.clone());
    Ok(Some(HitlEventFields {
        interrupt_id,
        tool_name,
        tool_call_id,
        status,
        permission,
        patterns,
        always,
        metadata,
    }))
}

async fn publish_hitl_pending_snapshot(
    db: &PgPool,
    bus: &AgentRuntimeTaskEventBus,
    conversation_id: &str,
    event_type: &str,
    interrupt_id: Option<&str>,
    decision: Option<&str>,
    comment: Option<&str>,
) -> Result<(), sqlx_core::error::Error> {
    let items = query_hitl_pending(
        db,
        &NormalizedHitlPendingQuery {
            conversation_id: conversation_id.trim().to_string(),
            status: "pending".to_string(),
            status_all: false,
            page: 1,
            page_size: 200,
        },
    )
    .await?;
    let event_fields = if let Some(interrupt_id) = interrupt_id {
        query_hitl_event_fields(db, interrupt_id).await?
    } else {
        None
    };
    let event = hitl_state_event(
        conversation_id,
        event_type,
        items,
        event_fields,
        interrupt_id,
        decision,
        comment,
    );
    let _ = save_and_publish_state_event(db, bus, event).await?;
    Ok(())
}

fn hitl_state_event(
    conversation_id: &str,
    event_type: &str,
    items: Vec<HitlPendingItem>,
    event_fields: Option<HitlEventFields>,
    interrupt_id: Option<&str>,
    decision: Option<&str>,
    comment: Option<&str>,
) -> NormalizedAgentRuntimeTaskEventInput {
    let conversation_id = conversation_id.trim().to_string();
    let event_type = event_type.trim();
    let event_type = if event_type == "hitl_decision_updated" {
        "hitl_decision_updated"
    } else {
        "hitl_pending_updated"
    };
    let fields = event_fields.unwrap_or_else(|| HitlEventFields {
        interrupt_id: interrupt_id.unwrap_or_default().to_string(),
        tool_name: String::new(),
        tool_call_id: String::new(),
        status: String::new(),
        permission: String::new(),
        patterns: Vec::new(),
        always: false,
        metadata: Value::Null,
    });
    NormalizedAgentRuntimeTaskEventInput {
        conversation_id: conversation_id.clone(),
        line: normalize_sse_line(format!(
            "data: {}",
            json!({
                "type": event_type,
                "message": decision.unwrap_or_default(),
                "data": {
                    "conversationId": conversation_id,
                    "items": items,
                    "interruptId": fields.interrupt_id,
                    "requestId": fields.interrupt_id,
                    "toolName": fields.tool_name,
                    "toolCallId": fields.tool_call_id,
                    "status": fields.status,
                    "decision": decision.unwrap_or_default(),
                    "comment": comment.unwrap_or_default(),
                    "permission": fields.permission,
                    "patterns": fields.patterns,
                    "always": fields.always,
                    "metadata": fields.metadata,
                }
            })
        )),
        runtime_event_id: String::new(),
        event_type: event_type.to_string(),
        terminal: false,
    }
}

async fn load_config(db: &PgPool) -> Result<Option<Value>, sqlx_core::error::Error> {
    query_scalar::<_, Value>("SELECT value FROM app_config WHERE key = $1")
        .bind(FRONTEND_CONFIG_KEY)
        .fetch_optional(db)
        .await
        .map(|value| value.map(frontend_config_projection))
}

async fn load_or_initialize_config(db: &PgPool) -> Result<Value, sqlx_core::error::Error> {
    if let Some(value) = load_config(db).await? {
        return Ok(value);
    }
    let value = default_frontend_config();
    let row = query(
        r#"
        INSERT INTO app_config (key, value, updated_at)
        VALUES ($1, $2, NOW())
        ON CONFLICT (key) DO NOTHING
        RETURNING value
        "#,
    )
    .bind(FRONTEND_CONFIG_KEY)
    .bind(&value)
    .fetch_optional(db)
    .await?;
    if let Some(row) = row {
        return Row::try_get(&row, "value");
    }
    Ok(load_config(db).await?.unwrap_or(value))
}

async fn save_config(db: &PgPool, value: &Value) -> Result<(), sqlx_core::error::Error> {
    let value = frontend_config_projection(value.clone());
    query(
        r#"
        INSERT INTO app_config (key, value, updated_at)
        VALUES ($1, $2, NOW())
        ON CONFLICT (key)
        DO UPDATE SET value = EXCLUDED.value, updated_at = NOW()
        "#,
    )
    .bind(FRONTEND_CONFIG_KEY)
    .bind(&value)
    .execute(db)
    .await?;
    Ok(())
}

async fn ensure_messages_mcp_execution_ids_jsonb(
    db: &PgPool,
) -> Result<(), sqlx_core::error::Error> {
    let data_type = query_scalar::<_, String>(
        r#"
        SELECT data_type
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'messages'
          AND column_name = 'mcp_execution_ids'
        "#,
    )
    .fetch_optional(db)
    .await?;
    if data_type.as_deref() == Some("jsonb") {
        return Ok(());
    }
    query(
        r#"
        ALTER TABLE messages
        ALTER COLUMN mcp_execution_ids DROP DEFAULT,
        ALTER COLUMN mcp_execution_ids TYPE JSONB
            USING COALESCE(NULLIF(mcp_execution_ids, ''), '[]')::jsonb,
        ALTER COLUMN mcp_execution_ids SET DEFAULT '[]'::jsonb,
        ALTER COLUMN mcp_execution_ids SET NOT NULL
        "#,
    )
    .execute(db)
    .await?;
    Ok(())
}

fn default_frontend_config() -> Value {
    json!({
        "openai": {
            "provider": "openai",
            "base_url": "https://api.openai.com/v1",
            "api_key": "",
            "model": "gpt-4",
            "reasoning": {
                "effort": "xhigh"
            }
        }
    })
}

fn frontend_config_projection(value: Value) -> Value {
    let default = default_frontend_config();
    let mut out = default.clone();

    if let Some(openai) = value.get("openai").and_then(Value::as_object) {
        let out_openai = out
            .get_mut("openai")
            .and_then(Value::as_object_mut)
            .expect("default openai object");
        for key in ["provider", "base_url", "api_key", "model"] {
            if let Some(raw) = openai.get(key) {
                out_openai.insert(key.to_string(), raw.clone());
            }
        }

        if let Some(reasoning) = openai.get("reasoning").and_then(Value::as_object) {
            let out_reasoning = out_openai
                .get_mut("reasoning")
                .and_then(Value::as_object_mut)
                .expect("default reasoning object");
            if let Some(raw) = reasoning.get("effort") {
                out_reasoning.insert("effort".to_string(), raw.clone());
            }
        }
    }

    out
}

fn frontend_config_patch_projection(value: Value) -> Value {
    let mut out = json!({});
    if let Some(openai) = value.get("openai").and_then(Value::as_object) {
        let mut out_openai = serde_json::Map::new();
        for key in ["provider", "base_url", "api_key", "model"] {
            if let Some(raw) = openai.get(key) {
                out_openai.insert(key.to_string(), raw.clone());
            }
        }

        if let Some(reasoning) = openai.get("reasoning").and_then(Value::as_object) {
            let mut out_reasoning = serde_json::Map::new();
            if let Some(raw) = reasoning.get("effort") {
                out_reasoning.insert("effort".to_string(), raw.clone());
            }
            if !out_reasoning.is_empty() {
                out_openai.insert("reasoning".to_string(), Value::Object(out_reasoning));
            }
        }

        if !out_openai.is_empty() {
            out["openai"] = Value::Object(out_openai);
        }
    }
    out
}

fn parse_list_models_request(body: &[u8]) -> Result<ListModelsRequest, ApiError> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(ListModelsRequest {
            provider: None,
            base_url: None,
            api_key: None,
        });
    }
    serde_json::from_slice(body)
        .map_err(|err| ApiError::bad_request(format!("无效的请求参数: {err}")))
}

fn first_non_empty<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> String {
    for value in values {
        let Some(value) = value else {
            continue;
        };
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    String::new()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedProjectsQuery {
    status: String,
    search: String,
    limit: i64,
    offset: i64,
}

impl ListProjectsQuery {
    fn normalized(self) -> NormalizedProjectsQuery {
        let limit = self.limit.unwrap_or(50).clamp(1, 500);
        let offset = self.offset.unwrap_or(0).max(0);
        NormalizedProjectsQuery {
            status: self.status.unwrap_or_default().trim().to_string(),
            search: self.search.unwrap_or_default().trim().to_string(),
            limit,
            offset,
        }
    }
}

impl NormalizedProjectsQuery {
    fn search_pattern(&self) -> String {
        if self.search.is_empty() {
            String::new()
        } else {
            format!("%{}%", self.search)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedHitlPendingQuery {
    conversation_id: String,
    status: String,
    status_all: bool,
    page: i64,
    page_size: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedHitlInterruptInput {
    id: String,
    conversation_id: String,
    message_id: String,
    mode: String,
    tool_name: String,
    tool_call_id: String,
    payload: String,
    status: String,
}

impl ListHitlPendingQuery {
    fn normalized(self) -> NormalizedHitlPendingQuery {
        let status = self
            .status
            .unwrap_or_else(|| "pending".to_string())
            .trim()
            .to_string();
        let status = if status.is_empty() {
            "pending".to_string()
        } else {
            status
        };
        let page = self.page.unwrap_or(1).max(1);
        let page_size = self.page_size.unwrap_or(20).clamp(1, 200);
        NormalizedHitlPendingQuery {
            conversation_id: self.conversation_id.unwrap_or_default().trim().to_string(),
            status_all: status == "all",
            status,
            page,
            page_size,
        }
    }
}

fn normalize_hitl_interrupt_input(
    req: UpsertHitlInterruptRequest,
) -> Result<NormalizedHitlInterruptInput, ApiError> {
    let id = req.id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::bad_request("id is required"));
    }
    let conversation_id = req.conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Err(ApiError::bad_request("conversationId is required"));
    }
    let tool_name = req.tool_name.trim().to_string();
    if tool_name.is_empty() {
        return Err(ApiError::bad_request("toolName is required"));
    }
    let status = req
        .status
        .unwrap_or_else(|| "pending".to_string())
        .trim()
        .to_string();
    let status = if status.is_empty() {
        "pending".to_string()
    } else {
        status
    };
    if !matches!(
        status.as_str(),
        "pending" | "decided" | "timeout" | "cancelled"
    ) {
        return Err(ApiError::bad_request("invalid status"));
    }
    Ok(NormalizedHitlInterruptInput {
        id,
        conversation_id,
        message_id: req
            .message_id
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        mode: normalize_hitl_mode(&req.mode),
        tool_name,
        tool_call_id: req
            .tool_call_id
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        payload: req.payload.unwrap_or_default(),
        status,
    })
}

async fn save_hitl_interrupt(
    db: &PgPool,
    interrupt: &NormalizedHitlInterruptInput,
) -> Result<(), sqlx_core::error::Error> {
    query(
        r#"
        INSERT INTO hitl_interrupts
            (id, conversation_id, message_id, mode, tool_name, tool_call_id, payload, status, created_at)
        VALUES ($1, $2, NULLIF($3, ''), $4, $5, NULLIF($6, ''), $7, $8, NOW())
        ON CONFLICT (id)
        DO UPDATE SET conversation_id = EXCLUDED.conversation_id,
                      message_id = EXCLUDED.message_id,
                      mode = EXCLUDED.mode,
                      tool_name = EXCLUDED.tool_name,
                      tool_call_id = EXCLUDED.tool_call_id,
                      payload = EXCLUDED.payload,
                      status = CASE WHEN hitl_interrupts.status = 'pending' THEN EXCLUDED.status ELSE hitl_interrupts.status END
        "#,
    )
    .bind(&interrupt.id)
    .bind(&interrupt.conversation_id)
    .bind(&interrupt.message_id)
    .bind(&interrupt.mode)
    .bind(&interrupt.tool_name)
    .bind(&interrupt.tool_call_id)
    .bind(&interrupt.payload)
    .bind(&interrupt.status)
    .execute(db)
    .await?;
    Ok(())
}

impl NormalizedHitlPendingQuery {
    fn offset(&self) -> i64 {
        (self.page - 1) * self.page_size
    }
}

fn default_hitl_config() -> HitlConfig {
    HitlConfig {
        enabled: false,
        mode: "off".to_string(),
        sensitive_tools: Vec::new(),
        timeout_seconds: 0,
    }
}

fn normalize_hitl_mode(mode: &str) -> String {
    match mode.trim().to_ascii_lowercase().as_str() {
        "" => "approval".to_string(),
        "off" => "off".to_string(),
        "feedback" | "followup" => "approval".to_string(),
        "approval" | "review_edit" => mode.trim().to_ascii_lowercase(),
        _ => "approval".to_string(),
    }
}

fn normalize_hitl_config(mut hitl: HitlConfig) -> HitlConfig {
    hitl.sensitive_tools = clean_string_list(hitl.sensitive_tools);
    hitl.timeout_seconds = hitl.timeout_seconds.max(0);
    hitl.mode = normalize_hitl_mode(&hitl.mode);
    if !hitl.enabled {
        hitl.mode = "off".to_string();
    }
    hitl
}

fn hitl_config_effective(hitl: &HitlConfig) -> bool {
    hitl.enabled || normalize_hitl_mode(&hitl.mode) != "off"
}

fn normalize_permission_reply(
    reply: Option<&str>,
    decision: &str,
) -> Result<PermissionReply, ApiError> {
    let raw = reply
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(decision)
        .trim()
        .to_ascii_lowercase();
    match raw.as_str() {
        "once" | "approve" | "approved" | "allow" => Ok(PermissionReply::Once),
        "always" => Ok(PermissionReply::Always),
        "reject" | "rejected" | "deny" => Ok(PermissionReply::Reject),
        _ => Err(ApiError::bad_request(
            "reply must be once/always/reject or decision approve/reject",
        )),
    }
}

fn permission_reply_decision(reply: &PermissionReply) -> &'static str {
    match reply {
        PermissionReply::Once | PermissionReply::Always => "approve",
        PermissionReply::Reject => "reject",
    }
}

fn normalize_permission_request(mut req: PermissionRequest) -> Result<PermissionRequest, ApiError> {
    req.id = req.id.trim().to_string();
    if req.id.is_empty() {
        return Err(ApiError::bad_request("id is required"));
    }
    req.conversation_id = req.conversation_id.trim().to_string();
    if req.conversation_id.is_empty() {
        return Err(ApiError::bad_request("conversationId is required"));
    }
    req.session_id = req.session_id.trim().to_string();
    req.message_id = req.message_id.trim().to_string();
    req.tool_name = req.tool_name.trim().to_string();
    if req.tool_name.is_empty() {
        return Err(ApiError::bad_request("toolName is required"));
    }
    req.tool_call_id = req.tool_call_id.trim().to_string();
    req.permission = req.permission.trim().to_string();
    if req.permission.is_empty() {
        req.permission = req.tool_name.clone();
    }
    req.patterns = clean_string_list(req.patterns);
    if req.patterns.is_empty() {
        req.patterns = permission_patterns(&req.permission, &req.tool_name);
    }
    req.timeout_seconds = req.timeout_seconds.max(0);
    Ok(req)
}

fn permission_session_key(conversation_id: &str, session_id: &str) -> String {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        conversation_id.trim().to_string()
    } else {
        format!("{}:{}", conversation_id.trim(), session_id)
    }
}

fn permission_request_patterns(req: &PermissionRequest) -> Vec<String> {
    if req.patterns.is_empty() {
        permission_patterns(&req.permission, &req.tool_name)
    } else {
        req.patterns.clone()
    }
}

fn permission_patterns(permission: &str, tool_name: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    for value in [permission, tool_name] {
        let trimmed = value.trim();
        if !trimmed.is_empty() && !patterns.iter().any(|p| p == trimmed) {
            patterns.push(trimmed.to_string());
        }
    }
    if patterns.is_empty() {
        patterns.push("*".to_string());
    }
    patterns
}

fn permission_request_payload(req: &PermissionRequest) -> Value {
    json!({
        "schema": "cyberstrike.hitl.permission.v1",
        "id": req.id,
        "conversationId": req.conversation_id,
        "sessionId": req.session_id,
        "messageId": req.message_id,
        "toolName": req.tool_name,
        "toolCallId": req.tool_call_id,
        "permission": req.permission,
        "patterns": req.patterns,
        "always": req.always,
        "metadata": req.metadata,
        "payload": req.payload,
    })
}

fn parse_payload_json(payload: &str) -> Value {
    serde_json::from_str(payload.trim()).unwrap_or(Value::Null)
}

fn hitl_permission_fields(
    payload: &Value,
    fallback_tool_name: String,
) -> (String, Vec<String>, bool, Value) {
    let permission = payload
        .get("permission")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback_tool_name.trim())
        .to_string();
    let patterns = payload
        .get("patterns")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|items| !items.is_empty())
        .unwrap_or_else(|| permission_patterns(&permission, &fallback_tool_name));
    let always = payload
        .get("always")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let metadata = payload.get("metadata").cloned().unwrap_or(Value::Null);
    (permission, patterns, always, metadata)
}

fn permission_rules_decide(
    state: &PermissionState,
    key: &str,
    permission: &str,
    patterns: &[String],
    tool_name: &str,
) -> Option<PermissionAction> {
    let rules = state.session_rules.get(key)?;
    let mut values = Vec::new();
    values.push(permission.trim().to_string());
    values.push(tool_name.trim().to_string());
    values.extend(patterns.iter().map(|item| item.trim().to_string()));
    values.retain(|item| !item.is_empty());
    rules
        .iter()
        .rev()
        .find(|rule| {
            (rule.permission.trim().is_empty()
                || rule.permission.trim() == permission.trim()
                || wildcard_match(rule.permission.trim(), permission.trim()))
                && values
                    .iter()
                    .any(|value| wildcard_match(rule.pattern.trim(), value))
        })
        .map(|rule| rule.action.clone())
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.trim();
    let value = value.trim();
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern.eq_ignore_ascii_case(value);
    }
    let mut rest = value;
    let mut anchored_start = true;
    for part in pattern.split('*') {
        if part.is_empty() {
            anchored_start = false;
            continue;
        }
        let Some(index) = rest.to_ascii_lowercase().find(&part.to_ascii_lowercase()) else {
            return false;
        };
        if anchored_start && index != 0 {
            return false;
        }
        rest = &rest[index + part.len()..];
        anchored_start = false;
    }
    pattern.ends_with('*') || rest.is_empty()
}

fn internal_base_url(listen: &SocketAddr) -> String {
    let host = match listen.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_string(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => "[::1]".to_string(),
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        ip => ip.to_string(),
    };
    format!("http://{}:{}", host, listen.port())
}

fn inject_permission_context(
    command: &mut Value,
    internal_base_url: &str,
    assistant_message_id: &str,
) {
    let Some(obj) = command.as_object_mut() else {
        return;
    };
    let context = obj
        .entry("context".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !context.is_object() {
        *context = Value::Object(Map::new());
    }
    let Some(context) = context.as_object_mut() else {
        return;
    };
    let base = internal_base_url.trim().trim_end_matches('/');
    if !base.is_empty() {
        context.insert(
            "hitl_permission_ask_url".to_string(),
            Value::String(format!("{base}/api/internal/hitl/permission-ask")),
        );
    }
    if !assistant_message_id.trim().is_empty() {
        context.insert(
            "assistant_message_id".to_string(),
            Value::String(assistant_message_id.trim().to_string()),
        );
        context.insert(
            "assistantMessageId".to_string(),
            Value::String(assistant_message_id.trim().to_string()),
        );
    }
}

fn clean_string_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn json_string_array(value: Value) -> Vec<String> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn role_item_from_value(key: String, value: Value, enabled: bool) -> RoleItem {
    let obj = value.as_object();
    let name = obj
        .and_then(|item| item.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .unwrap_or(key.trim())
        .to_string();
    let prompt = obj
        .and_then(|item| item.get("prompt"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let user_prompt = obj
        .and_then(|item| item.get("user_prompt"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    RoleItem {
        name,
        description: obj
            .and_then(|item| item.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        prompt,
        user_prompt,
        icon: obj
            .and_then(|item| item.get("icon"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        tools: obj
            .and_then(|item| item.get("tools"))
            .cloned()
            .map(json_string_array)
            .unwrap_or_default(),
        mcps: obj
            .and_then(|item| item.get("mcps"))
            .cloned()
            .map(json_string_array)
            .unwrap_or_default(),
        enabled,
    }
}

fn merge_json_objects(dst: &mut Value, src: Value) {
    let Some(dst_obj) = dst.as_object_mut() else {
        *dst = src;
        return;
    };
    let Value::Object(src_obj) = src else {
        *dst = src;
        return;
    };
    for (key, value) in src_obj {
        match (dst_obj.get_mut(&key), &value) {
            (Some(existing @ Value::Object(_)), Value::Object(_)) => {
                merge_json_objects(existing, value.clone());
            }
            _ => {
                dst_obj.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_config_objects_recursively() {
        let mut dst = json!({
            "openai": {
                "provider": "openai",
                "api_key": "k",
                "base_url": "http://base/v1",
                "model": "old",
                "reasoning": {"effort": "xhigh", "mode": "on"}
            }
        });
        merge_json_objects(
            &mut dst,
            json!({
                "openai": {
                    "model": "new",
                    "reasoning": {"effort": "low"}
                }
            }),
        );
        assert_eq!(dst["openai"]["provider"], "openai");
        assert_eq!(dst["openai"]["api_key"], "k");
        assert_eq!(dst["openai"]["model"], "new");
        assert_eq!(dst["openai"]["reasoning"]["effort"], "low");
        assert_eq!(dst["openai"]["reasoning"]["mode"], "on");
    }

    #[test]
    fn keeps_frontend_config_fields_only() {
        let projected = frontend_config_projection(json!({
            "openai": {
                "provider": "openai-compatible",
                "base_url": "http://pg/v1",
                "api_key": "secret",
                "model": "pg-model",
                "max_total_tokens": 999,
                "reasoning": {
                    "mode": "on",
                    "effort": "low",
                    "allow_client_reasoning": false
                }
            },
            "agent_runtime": {"enabled": true},
            "server": {"port": 51282},
            "auth": {"enabled": true},
            "database": {"path": "data/conversations.db"},
            "security": {"tools": []}
        }));

        assert_eq!(projected["openai"]["provider"], "openai-compatible");
        assert_eq!(projected["openai"]["base_url"], "http://pg/v1");
        assert_eq!(projected["openai"]["api_key"], "secret");
        assert_eq!(projected["openai"]["model"], "pg-model");
        assert_eq!(projected["openai"]["reasoning"]["effort"], "low");
        assert!(projected["openai"]["reasoning"].get("mode").is_none());
        assert!(projected.get("agent_runtime").is_none());
        assert!(projected.get("server").is_none());
        assert!(projected.get("auth").is_none());
        assert!(projected.get("database").is_none());
        assert!(projected.get("security").is_none());
        assert!(projected["openai"].get("max_total_tokens").is_none());
        assert!(projected["openai"]["reasoning"]
            .get("allow_client_reasoning")
            .is_none());
    }

    #[test]
    fn frontend_config_patch_projection_only_includes_present_fields() {
        let patch = frontend_config_patch_projection(json!({
            "openai": {
                "provider": "openai-compatible",
                "api_key": "secret",
                "base_url": "http://pg/v1",
                "model": "new-model",
                "max_total_tokens": 999,
                "reasoning": {
                    "mode": "ignored",
                    "effort": "low",
                    "allow_client_reasoning": false
                }
            },
            "agent_runtime": {"enabled": true},
            "vision": {"enabled": false},
            "tools": [{"name": "nmap", "enabled": false}]
        }));

        assert_eq!(
            patch,
            json!({
                "openai": {
                    "provider": "openai-compatible",
                    "api_key": "secret",
                    "base_url": "http://pg/v1",
                    "model": "new-model",
                    "reasoning": {"effort": "low"}
                }
            })
        );
    }

    #[test]
    fn frontend_config_patch_cannot_overwrite_fields_outside_chat_web_openai() {
        let mut next = json!({
            "openai": {
                "provider": "openai",
                "api_key": "old-key",
                "base_url": "http://old/v1",
                "model": "old-model",
                "reasoning": {"effort": "xhigh"}
            }
        });

        merge_json_objects(
            &mut next,
            frontend_config_patch_projection(json!({
                "openai": {
                    "model": "new-model",
                    "max_total_tokens": 999,
                    "reasoning": {
                        "effort": "low",
                        "mode": "on",
                        "profile": "ignored"
                    }
                },
                "agent_runtime": {"enabled": false},
                "vision": {"enabled": false},
                "server": {"port": 4177},
                "tools": [{"name": "nmap", "enabled": false}]
            })),
        );

        let projected = frontend_config_projection(next);
        assert_eq!(projected["openai"]["api_key"], "old-key");
        assert_eq!(projected["openai"]["base_url"], "http://old/v1");
        assert_eq!(projected["openai"]["model"], "new-model");
        assert_eq!(projected["openai"]["reasoning"]["effort"], "low");
        assert!(projected["openai"].get("max_total_tokens").is_none());
        assert!(projected["openai"]["reasoning"].get("mode").is_none());
        assert!(projected["openai"]["reasoning"].get("profile").is_none());
        assert!(projected.get("agent_runtime").is_none());
        assert!(projected.get("vision").is_none());
        assert!(projected.get("server").is_none());
        assert!(projected.get("tools").is_none());
    }

    #[test]
    fn parses_empty_list_models_body() {
        let req = parse_list_models_request(b" \n\t ").expect("empty body parses");
        assert!(req.provider.is_none());
        assert!(req.api_key.is_none());
    }

    #[test]
    fn parses_list_models_snake_case_overrides() {
        let req = parse_list_models_request(
            br#"{"provider":"openai","base_url":"http://base/v1","api_key":"secret"}"#,
        )
        .expect("parse body");
        assert_eq!(req.provider.as_deref(), Some("openai"));
        assert_eq!(req.base_url.as_deref(), Some("http://base/v1"));
        assert_eq!(req.api_key.as_deref(), Some("secret"));
    }

    #[test]
    fn first_non_empty_trims_values() {
        let got = first_non_empty([None, Some("  "), Some(" openai "), Some("ignored")]);
        assert_eq!(got, "openai");
    }

    #[test]
    fn sorted_model_ids_are_unique() {
        let mut models: Vec<String> = vec![
            "z-model".to_string(),
            "a-model".to_string(),
            "z-model".to_string(),
        ];
        models.sort();
        models.dedup();
        assert_eq!(models, vec!["a-model".to_string(), "z-model".to_string()]);
    }

    #[test]
    fn normalizes_agent_runtime_task_defaults() {
        let got = normalize_agent_runtime_task_input(
            " conv-1 ".to_string(),
            UpsertAgentRuntimeTaskRequest {
                message: Some(" hello ".to_string()),
                status: None,
                agent_mode: None,
                assistant_message_id: Some(" assistant-1 ".to_string()),
                started_at: None,
                active: None,
            },
        )
        .expect("normalize task");
        assert_eq!(
            got,
            NormalizedAgentRuntimeTaskInput {
                conversation_id: "conv-1".to_string(),
                message: "hello".to_string(),
                status: "running".to_string(),
                agent_mode: "agent_runtime".to_string(),
                assistant_message_id: "assistant-1".to_string(),
                started_at: "".to_string(),
                active: true,
            }
        );
    }

    #[test]
    fn normalizes_agent_runtime_task_finished() {
        let got = normalize_agent_runtime_task_input(
            "conv-1".to_string(),
            UpsertAgentRuntimeTaskRequest {
                message: None,
                status: Some(" completed ".to_string()),
                agent_mode: Some(" agent_runtime ".to_string()),
                assistant_message_id: None,
                started_at: Some("2026-06-25T20:00:00Z".to_string()),
                active: Some(false),
            },
        )
        .expect("normalize task");
        assert_eq!(got.status, "completed");
        assert_eq!(got.agent_mode, "agent_runtime");
        assert_eq!(got.started_at, "2026-06-25T20:00:00Z");
        assert!(!got.active);
    }

    #[test]
    fn normalizes_agent_runtime_task_events_query_from_header() {
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", HeaderValue::from_static("41"));
        let q = AgentRuntimeTaskEventsQuery {
            conversation_id: Some(" conv-1 ".to_string()),
            after_event_id: None,
            runtime_event_id: None,
            limit: Some(5000),
        }
        .normalized(&headers);

        assert_eq!(
            q,
            NormalizedAgentRuntimeTaskEventsQuery {
                conversation_id: "conv-1".to_string(),
                scoped_to_conversation: true,
                after_id: 41,
                after_runtime_event_id: String::new(),
                limit: 1000,
            }
        );
    }

    #[test]
    fn normalizes_unscoped_agent_runtime_task_events_query() {
        let q = AgentRuntimeTaskEventsQuery {
            conversation_id: None,
            after_event_id: Some(" 5 ".to_string()),
            runtime_event_id: None,
            limit: Some(7),
        }
        .normalized(&HeaderMap::new());

        assert_eq!(
            q,
            NormalizedAgentRuntimeTaskEventsQuery {
                conversation_id: "".to_string(),
                scoped_to_conversation: false,
                after_id: 5,
                after_runtime_event_id: String::new(),
                limit: 7,
            }
        );
    }

    #[test]
    fn normalizes_agent_runtime_task_events_query_keeps_runtime_cursor_when_no_sse_id() {
        let q = AgentRuntimeTaskEventsQuery {
            conversation_id: Some("conv-1".to_string()),
            after_event_id: None,
            runtime_event_id: Some("1740000000000-0".to_string()),
            limit: None,
        }
        .normalized(&HeaderMap::new());

        assert_eq!(q.after_id, 0);
        assert_eq!(q.after_runtime_event_id, "1740000000000-0");
    }

    #[test]
    fn normalizes_agent_runtime_task_events_query_prefers_last_event_id() {
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", HeaderValue::from_static("42"));
        let q = AgentRuntimeTaskEventsQuery {
            conversation_id: None,
            after_event_id: Some("1740000000000-0".to_string()),
            runtime_event_id: None,
            limit: None,
        }
        .normalized(&headers);

        assert_eq!(q.after_id, 42);
        assert!(q.after_runtime_event_id.is_empty());
    }

    #[test]
    fn normalizes_agent_runtime_task_event_input() {
        let got = normalize_agent_runtime_task_event_input(CreateAgentRuntimeTaskEventRequest {
            conversation_id: " conv-1 ".to_string(),
            line: "data: {\"type\":\"progress\"}".to_string(),
            runtime_event_id: Some(" 3-0 ".to_string()),
            event_type: Some(" progress ".to_string()),
            terminal: Some(false),
        })
        .expect("normalize event");

        assert_eq!(
            got,
            NormalizedAgentRuntimeTaskEventInput {
                conversation_id: "conv-1".to_string(),
                line: "data: {\"type\":\"progress\"}\n\n".to_string(),
                runtime_event_id: "3-0".to_string(),
                event_type: "progress".to_string(),
                terminal: false,
            }
        );
    }

    #[test]
    fn builds_agent_runtime_task_event_sse_frame() {
        let frame = agent_runtime_task_event_sse_frame(&StoredAgentRuntimeTaskEvent {
            id: 7,
            conversation_id: "conv-1".to_string(),
            line: "data: {\"type\":\"done\"}\n\n".to_string(),
            event_type: "done".to_string(),
            terminal: true,
        });

        assert!(frame.starts_with("id: 7\n"));
        assert!(frame.ends_with("\n\n"));
        assert!(frame.contains(r#""type":"done""#));
    }

    #[test]
    fn normalizes_stored_command_completed_task_events_for_frontend() {
        let line = normalize_agent_runtime_task_event_sse_line_for_frontend(
            r#"data: {"type":"done","message":"","data":{"conversationId":"conv-1","runtimeEventType":"command_completed","runtimeTrace":{"type":"command_completed"}}}"#,
        );

        assert!(line.ends_with("\n\n"));
        assert!(line.contains(r#""type":"done""#));
        assert!(line.contains(r#""runtimeEventType":"done""#));
        assert!(line.contains(r#""runtimeRawEventType":"command_completed""#));
        assert!(line.contains(r#""conversationId":"conv-1""#));
    }

    #[test]
    fn maps_runtime_event_identity_and_frontend_sse_line() {
        let raw =
            r#"{"type":"assistant_delta","event_id":"evt-1","delta":"hi","accumulated":"hi"}"#;
        let (event_type, runtime_event_id, terminal) = runtime_event_identity_from_json(raw);
        assert_eq!(event_type, "assistant_delta");
        assert_eq!(runtime_event_id, "evt-1");
        assert!(!terminal);

        let line = runtime_event_json_to_sse_line("conv-1", raw);
        assert!(line.starts_with("data: "));
        assert!(line.ends_with("\n\n"));
        assert!(line.contains(r#""type":"response_delta""#));
        assert!(line.contains(r#""conversationId":"conv-1""#));
        assert!(line.contains(r#""runtimeEventId":"evt-1""#));
    }

    #[test]
    fn maps_runtime_turn_completed_to_terminal_frontend_sse_line() {
        let line = runtime_event_json_to_sse_line(
            "conv-1",
            r#"{"type":"turn_completed","response":"ok"}"#,
        );

        assert!(line.contains(r#""type":"turn_completed""#));
        assert!(line.contains(r#""runtimeEventType":"turn_completed""#));
        assert!(line.contains(r#""message":"ok""#));
    }

    #[test]
    fn extracts_final_response_from_assistant_delta_accumulated() {
        let got = agent_runtime_final_response_from_sse_line(
            r#"data: {"type":"response_delta","message":"o","data":{"conversationId":"conv-1","runtimeEventType":"assistant_delta","accumulated":"hello"}}"#,
        )
        .expect("final response candidate");

        assert!(!got.completed);
        assert_eq!(got.accumulated, "hello");
        assert_eq!(got.response, "");
    }

    #[test]
    fn extracts_final_response_from_turn_completed_response() {
        let got = agent_runtime_final_response_from_sse_line(
            r#"data: {"type":"progress","message":"Agent Runtime turn 已完成","data":{"conversationId":"conv-1","runtimeEventType":"turn_completed","runtimeTrace":{"type":"turn_completed","response":"final answer"}}}"#,
        )
        .expect("final response candidate");

        assert!(got.completed);
        assert_eq!(got.response, "final answer");
        assert_eq!(got.accumulated, "");
    }

    #[test]
    fn final_response_uses_latest_completed_turn() {
        let first_start = r#"data: {"type":"progress","data":{"conversationId":"conv-1","runtimeEventType":"turn_started","runtimeTrace":{"type":"turn_started"}}}"#;
        let first_done = r#"data: {"type":"progress","message":"Agent Runtime turn 已完成","data":{"conversationId":"conv-1","runtimeEventType":"turn_completed","runtimeTrace":{"type":"turn_completed","response":"first answer"}}}"#;
        let second_start = r#"data: {"type":"progress","data":{"conversationId":"conv-1","runtimeEventType":"turn_started","runtimeTrace":{"type":"turn_started"}}}"#;
        let second_delta = r#"data: {"type":"response_delta","message":"second","data":{"conversationId":"conv-1","runtimeEventType":"assistant_delta","runtimeTrace":{"type":"assistant_delta","accumulated":"second partial"}}}"#;
        let second_done = r#"data: {"type":"progress","message":"Agent Runtime turn 已完成","data":{"conversationId":"conv-1","runtimeEventType":"turn_completed","runtimeTrace":{"type":"turn_completed","response":"second answer"}}}"#;

        let got = agent_runtime_final_response_from_sse_lines([
            first_start,
            first_done,
            second_start,
            second_delta,
            second_done,
        ]);

        assert_eq!(got, "second answer");
    }

    #[test]
    fn marks_runtime_turn_start_for_final_response_boundaries() {
        let got = agent_runtime_final_response_from_sse_line(
            r#"data: {"type":"progress","message":"","data":{"conversationId":"conv-1","runtimeEventType":"turn_started","runtimeTrace":{"type":"turn_started"}}}"#,
        )
        .expect("turn start candidate");

        assert!(got.starts_turn);
        assert!(!got.completed);
    }

    #[test]
    fn maps_runtime_command_completed_to_frontend_done_sse_line() {
        let line = runtime_event_json_to_sse_line(
            "conv-1",
            r#"{"type":"command_completed","event_id":"evt-command"}"#,
        );

        assert!(line.contains(r#""type":"done""#));
        assert!(line.contains(r#""runtimeEventType":"done""#));
        assert!(line.contains(r#""runtimeRawEventType":"command_completed""#));
        assert!(line.contains(r#""runtimeEventId":"evt-command""#));
    }

    #[test]
    fn builds_agent_runtime_task_state_events() {
        let running = agent_runtime_task_state_event(
            " conv-1 ",
            json!({
                "conversationId": "conv-1",
                "status": "running",
                "active": true
            }),
            false,
        );

        assert_eq!(running.event_type, "task_updated");
        assert!(!running.terminal);
        assert!(running.line.contains(r#""type":"task_updated""#));
        assert!(running.line.contains(r#""status":"running""#));

        let completed = agent_runtime_task_state_event(
            "conv-1",
            json!({
                "conversationId": "conv-1",
                "status": "completed",
                "active": false
            }),
            true,
        );

        assert_eq!(completed.event_type, "task_completed");
        assert!(completed.terminal);
        assert!(completed.line.contains(r#""type":"task_completed""#));
    }

    #[test]
    fn extracts_runtime_todos_from_plan_updated_event() {
        let event = NormalizedAgentRuntimeTaskEventInput {
            conversation_id: "conv-1".to_string(),
            line: normalize_sse_line(
                r#"data: {"type":"planning","message":"Todo/计划状态已更新。","data":{"conversationId":"conv-1","runtimeEventType":"plan_updated","runtimeTrace":{"type":"plan_updated","items":[{"id":"todo-1","content":"Check target","status":"running"},{"id":"todo-2","step":"Report","status":"done"}]}}}"#
                    .to_string(),
            ),
            runtime_event_id: "evt-plan".to_string(),
            event_type: "plan_updated".to_string(),
            terminal: false,
        };

        let got = runtime_todos_from_task_event(&event).expect("todos");

        assert_eq!(got.len(), 2);
        assert_eq!(got[0].item_id, "todo-1");
        assert_eq!(got[0].content, "Check target");
        assert_eq!(got[0].status, "in_progress");
        assert_eq!(got[1].content, "Report");
        assert_eq!(got[1].status, "completed");
    }

    #[test]
    fn extracts_runtime_todos_from_update_plan_markdown() {
        let event = NormalizedAgentRuntimeTaskEventInput {
            conversation_id: "conv-1".to_string(),
            line: normalize_sse_line(
                r#"data: {"type":"planning","message":"- [ ] Inspect\n- [>] Exploit\n- [x] Report","data":{"conversationId":"conv-1","runtimeEventType":"update_plan","runtimeTrace":{"type":"update_plan"}}}"#
                    .to_string(),
            ),
            runtime_event_id: "evt-plan".to_string(),
            event_type: "update_plan".to_string(),
            terminal: false,
        };

        let got = runtime_todos_from_task_event(&event).expect("todos");

        assert_eq!(
            got.iter()
                .map(|item| item.status.as_str())
                .collect::<Vec<_>>(),
            vec!["pending", "in_progress", "completed"]
        );
    }

    #[test]
    fn ignores_runtime_events_without_todos() {
        let event = NormalizedAgentRuntimeTaskEventInput {
            conversation_id: "conv-1".to_string(),
            line: normalize_sse_line(
                r#"data: {"type":"progress","message":"Working","data":{"conversationId":"conv-1","runtimeEventType":"assistant_progress_update","runtimeTrace":{"type":"assistant_progress_update","message":"Working"}}}"#
                    .to_string(),
            ),
            runtime_event_id: "evt-progress".to_string(),
            event_type: "assistant_progress_update".to_string(),
            terminal: false,
        };

        assert!(runtime_todos_from_task_event(&event).is_none());
    }

    #[test]
    fn mirrors_runtime_task_event_to_process_detail() {
        let event = NormalizedAgentRuntimeTaskEventInput {
            conversation_id: "conv-1".to_string(),
            line: normalize_sse_line(
                r#"data: {"type":"planning","message":"Todo/计划状态已更新。","data":{"conversationId":"conv-1","runtimeEventType":"plan_updated","runtimeTrace":{"type":"plan_updated","items":[{"id":"todo-1","content":"Check target","status":"in_progress"}]}}}"#
                    .to_string(),
            ),
            runtime_event_id: "evt-plan".to_string(),
            event_type: "plan_updated".to_string(),
            terminal: false,
        };

        let got = agent_runtime_process_detail_from_task_event(42, &event, "assistant-1")
            .expect("process detail");

        assert_eq!(got.id, "agent-runtime-task-event-42");
        assert_eq!(got.message_id, "assistant-1");
        assert_eq!(got.conversation_id, "conv-1");
        assert_eq!(got.event_type, "planning");
        assert_eq!(got.message, "Todo/计划状态已更新。");
        assert_eq!(got.data["assistantMessageId"], "assistant-1");
        assert_eq!(got.data["runtimeEventType"], "plan_updated");
        assert_eq!(
            got.data["items"][0],
            json!({"id":"todo-1","content":"Check target","status":"in_progress"})
        );
    }

    #[test]
    fn mirrors_runtime_assistant_progress_to_process_detail() {
        let event = NormalizedAgentRuntimeTaskEventInput {
            conversation_id: "conv-1".to_string(),
            line: normalize_sse_line(
                r#"data: {"type":"assistant_progress_update","message":"Inspecting files","data":{"conversationId":"conv-1","runtimeEventType":"assistant_progress_update","runtimeTrace":{"type":"assistant_progress_update","turn_id":"turn-1","message":"Inspecting files"}}}"#
                    .to_string(),
            ),
            runtime_event_id: "evt-progress".to_string(),
            event_type: "assistant_progress_update".to_string(),
            terminal: false,
        };

        let got = agent_runtime_process_detail_from_task_event(43, &event, "assistant-1")
            .expect("process detail");

        assert_eq!(got.event_type, "assistant_progress_update");
        assert_eq!(got.message, "Inspecting files");
        assert_eq!(got.data["assistantMessageId"], "assistant-1");
        assert_eq!(got.data["turnId"], "turn-1");
    }

    #[test]
    fn skips_non_process_runtime_task_events() {
        let event = NormalizedAgentRuntimeTaskEventInput {
            conversation_id: "conv-1".to_string(),
            line: normalize_sse_line(
                r#"data: {"type":"conversation_title_updated","message":"Title","data":{"conversationId":"conv-1","title":"Title"}}"#
                    .to_string(),
            ),
            runtime_event_id: "evt-title".to_string(),
            event_type: "conversation_title_updated".to_string(),
            terminal: false,
        };

        assert!(agent_runtime_process_detail_from_task_event(44, &event, "assistant-1").is_none());
    }

    #[test]
    fn marks_runtime_terminal_events() {
        let (_, _, terminal) = runtime_event_identity_from_json(
            r#"{"type":"turn_completed","runtime_event_id":"evt-done"}"#,
        );
        assert!(terminal);
    }

    #[test]
    fn normalizes_agent_runtime_stream_input() {
        let got = normalize_agent_runtime_stream_input(AcceptAgentRuntimeStreamRequest {
            conversation_id: " conv-1 ".to_string(),
            message: Some(" hello ".to_string()),
            project_id: None,
            role: None,
            reasoning: None,
            hitl: None,
            attachments: None,
            webshell_connection_id: None,
            agent_mode: None,
            assistant_message_id: Some(" assistant-1 ".to_string()),
            user_message_id: Some(" user-1 ".to_string()),
            started_at: Some("2026-06-25T20:00:00Z".to_string()),
            created_new: Some(true),
            background: None,
            runtime_binary_path: Some(" /opt/runtime ".to_string()),
            runtime_work_dir: Some(" /workspace ".to_string()),
            runtime_command: Some(json!({"type": "start_turn"})),
        })
        .expect("normalize stream");

        assert_eq!(
            got,
            NormalizedAgentRuntimeStreamRunInput {
                conversation_id: "conv-1".to_string(),
                message: "hello".to_string(),
                agent_mode: "agent_runtime".to_string(),
                assistant_message_id: "assistant-1".to_string(),
                user_message_id: "user-1".to_string(),
                started_at: "2026-06-25T20:00:00Z".to_string(),
                created_new: true,
                background: true,
                runtime_binary_path: "/opt/runtime".to_string(),
                runtime_work_dir: "/workspace".to_string(),
                runtime_command: Some(json!({"type": "start_turn"})),
            }
        );
    }

    #[test]
    fn rust_owned_runtime_command_enables_filesystem_tools_when_work_dir_is_set() {
        let runtime = RuntimeSettings {
            binary_path: "/opt/cyberstrike-agent-runtime".to_string(),
            work_dir: "/workspace/cyberstrike".to_string(),
            max_steps: 25,
            tool_timeout_seconds: 45,
            mcp_endpoint_url: String::new(),
            mcp_auth_header: String::new(),
            mcp_auth_header_value: String::new(),
            skills_dir: String::new(),
        };
        let command = build_agent_runtime_start_turn_command(
            &runtime,
            &json!({"openai": {"provider": "openai", "model": "gpt-test"}}),
            "conv-1",
            "assistant-1",
            "hello",
            "",
            "默认",
            "",
            &[],
            &[],
            None,
            &default_hitl_config(),
        );
        let context = command
            .get("context")
            .and_then(Value::as_object)
            .expect("runtime context");

        assert_eq!(
            context.get("workspace_root").and_then(Value::as_str),
            Some("/workspace/cyberstrike")
        );
        assert_eq!(
            context.get("filesystem_enabled").and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn builds_agent_runtime_stream_accepted_sse_response() {
        let response =
            agent_runtime_stream_accepted_sse_response(&NormalizedAgentRuntimeStreamRunInput {
                conversation_id: "conv-1".to_string(),
                message: "hello".to_string(),
                agent_mode: "agent_runtime".to_string(),
                assistant_message_id: "assistant-1".to_string(),
                user_message_id: "user-1".to_string(),
                started_at: "".to_string(),
                created_new: false,
                background: true,
                runtime_binary_path: "".to_string(),
                runtime_work_dir: "".to_string(),
                runtime_command: None,
            });

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream; charset=utf-8")
        );
    }

    #[test]
    fn rejects_agent_runtime_task_without_conversation_id() {
        let err = normalize_agent_runtime_task_input(
            "   ".to_string(),
            UpsertAgentRuntimeTaskRequest {
                message: None,
                status: None,
                agent_mode: None,
                assistant_message_id: None,
                started_at: None,
                active: None,
            },
        )
        .expect_err("empty conversation id rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn builds_agent_runtime_cancel_response() {
        let got = cancel_agent_runtime_response("conv-1".to_string(), " stop ".to_string());
        assert_eq!(
            got,
            CancelAgentRuntimeTaskResponse {
                status: "cancelling".to_string(),
                conversation_id: "conv-1".to_string(),
                message: "已提交取消请求，任务将在当前步骤完成后停止。".to_string(),
                continue_after: false,
                interrupt_with_note: true,
                agent_mode: "agent_runtime".to_string(),
            }
        );
    }

    #[test]
    fn builds_agent_runtime_cancellation_terminal_event() {
        let got =
            agent_runtime_cancellation_task_event(" conv-1 ", "Agent Runtime 已取消", "cancelled");

        assert_eq!(got.conversation_id, "conv-1");
        assert_eq!(got.event_type, "turn_aborted");
        assert!(got.terminal);
        assert!(got.runtime_event_id.is_empty());
        assert!(got.line.ends_with("\n\n"));
        assert!(got.line.contains(r#""type":"cancelled""#));
        assert!(got.line.contains(r#""conversationId":"conv-1""#));
        assert!(got.line.contains(r#""runtimeEventType":"turn_aborted""#));
        assert!(got.line.contains(r#""reason":"cancelled""#));
    }

    #[test]
    fn builds_agent_runtime_completed_terminal_event() {
        let got = agent_runtime_completed_task_event(" conv-1 ");

        assert_eq!(got.conversation_id, "conv-1");
        assert_eq!(got.event_type, "command_completed");
        assert!(got.terminal);
        assert!(got.runtime_event_id.is_empty());
        assert!(got.line.ends_with("\n\n"));
        assert!(got.line.contains(r#""type":"done""#));
        assert!(got.line.contains(r#""conversationId":"conv-1""#));
        assert!(got.line.contains(r#""runtimeEventType":"done""#));
        assert!(got.line.contains(r#""type":"command_completed""#));
    }

    #[test]
    fn normalizes_project_query_bounds() {
        let q = ListProjectsQuery {
            status: Some(" active ".to_string()),
            search: Some(" acme ".to_string()),
            limit: Some(900),
            offset: Some(-4),
        }
        .normalized();
        assert_eq!(
            q,
            NormalizedProjectsQuery {
                status: "active".to_string(),
                search: "acme".to_string(),
                limit: 500,
                offset: 0,
            }
        );
        assert_eq!(q.search_pattern(), "%acme%");
    }

    #[test]
    fn parses_json_string_arrays() {
        assert_eq!(
            json_string_array(json!([" nmap ", "", 7, "whoami"])),
            vec!["nmap".to_string(), "whoami".to_string()]
        );
        assert!(json_string_array(json!({"bad": true})).is_empty());
    }

    #[test]
    fn maps_role_item_from_json_value() {
        let role = role_item_from_value(
            "fallback".to_string(),
            json!({
                "name": "Analyst",
                "description": "desc",
                "prompt": "frontend prompt",
                "user_prompt": "runtime prompt",
                "icon": "shield",
                "tools": ["nmap", "", 7],
                "mcps": ["mcp"]
            }),
            true,
        );
        assert_eq!(role.name, "Analyst");
        assert_eq!(role.description, "desc");
        assert_eq!(role.prompt, "frontend prompt");
        assert_eq!(role.user_prompt, "runtime prompt");
        assert_eq!(role.icon, "shield");
        assert_eq!(role.tools, vec!["nmap".to_string()]);
        assert_eq!(role.mcps, vec!["mcp".to_string()]);
        assert!(role.enabled);

        let fallback = role_item_from_value("fallback".to_string(), json!({}), true);
        assert_eq!(fallback.name, "fallback");
    }

    #[test]
    fn normalizes_project_upsert_input() {
        let got = normalize_project_input(UpsertProjectRequest {
            id: " p1 ".to_string(),
            name: " Project ".to_string(),
            description: Some("desc".to_string()),
            scope_json: Some("{\"target\":\"example.com\"}".to_string()),
            status: Some("".to_string()),
            pinned: Some(true),
            created_at: Some("2026-06-25T20:00:00Z".to_string()),
            updated_at: Some("2026-06-25T21:00:00Z".to_string()),
        })
        .expect("normalize project");

        assert_eq!(
            got,
            NormalizedProjectInput {
                id: "p1".to_string(),
                name: "Project".to_string(),
                description: "desc".to_string(),
                scope_json: "{\"target\":\"example.com\"}".to_string(),
                status: "active".to_string(),
                pinned: true,
                created_at: "2026-06-25T20:00:00Z".to_string(),
                updated_at: "2026-06-25T21:00:00Z".to_string(),
            }
        );
    }

    #[test]
    fn normalizes_conversation_query_bounds() {
        let q = ListConversationsQuery {
            limit: Some(5000),
            offset: Some(-10),
            search: Some(" hello ".to_string()),
            sort_by: Some("created_at".to_string()),
        }
        .normalized();

        assert_eq!(
            q,
            NormalizedConversationsQuery {
                limit: 1000,
                offset: 0,
                search: "hello".to_string(),
                sort_by: "created_at".to_string(),
            }
        );
        assert_eq!(q.search_pattern(), "%hello%");
    }

    #[test]
    fn normalizes_conversation_upsert_input() {
        let got = normalize_conversation_input(UpsertConversationRequest {
            id: " conv-1 ".to_string(),
            title: " Conversation ".to_string(),
            project_id: Some(" project-1 ".to_string()),
            pinned: Some(true),
            created_at: Some("2026-06-26T00:00:00Z".to_string()),
            updated_at: Some("2026-06-26T01:00:00Z".to_string()),
        })
        .expect("normalize conversation");

        assert_eq!(
            got,
            NormalizedConversationInput {
                id: "conv-1".to_string(),
                title: "Conversation".to_string(),
                project_id: "project-1".to_string(),
                pinned: true,
                created_at: "2026-06-26T00:00:00Z".to_string(),
                updated_at: "2026-06-26T01:00:00Z".to_string(),
            }
        );
    }

    #[test]
    fn normalizes_message_upsert_input() {
        let got = normalize_message_input(UpsertMessageRequest {
            id: " msg-1 ".to_string(),
            conversation_id: " conv-1 ".to_string(),
            role: " assistant ".to_string(),
            content: "hello".to_string(),
            reasoning_content: Some("thinking".to_string()),
            mcp_execution_ids: Some(vec![" mcp-1 ".to_string(), "".to_string()]),
            created_at: Some("2026-06-26T00:00:00Z".to_string()),
            updated_at: Some("2026-06-26T01:00:00Z".to_string()),
        })
        .expect("normalize message");

        assert_eq!(
            got,
            NormalizedMessageInput {
                id: "msg-1".to_string(),
                conversation_id: "conv-1".to_string(),
                role: "assistant".to_string(),
                content: "hello".to_string(),
                reasoning_content: "thinking".to_string(),
                mcp_execution_ids: vec!["mcp-1".to_string()],
                created_at: "2026-06-26T00:00:00Z".to_string(),
                updated_at: "2026-06-26T01:00:00Z".to_string(),
            }
        );
    }

    #[test]
    fn normalizes_process_detail_upsert_input() {
        let got = normalize_process_detail_input(UpsertProcessDetailRequest {
            id: " pd-1 ".to_string(),
            message_id: " msg-1 ".to_string(),
            conversation_id: " conv-1 ".to_string(),
            event_type: " progress ".to_string(),
            message: Some("started".to_string()),
            data: Some(json!({"step": 1})),
            created_at: Some("2026-06-26T00:00:00Z".to_string()),
        })
        .expect("normalize process detail");

        assert_eq!(
            got,
            NormalizedProcessDetailInput {
                id: "pd-1".to_string(),
                message_id: "msg-1".to_string(),
                conversation_id: "conv-1".to_string(),
                event_type: "progress".to_string(),
                message: "started".to_string(),
                data: json!({"step": 1}),
                created_at: "2026-06-26T00:00:00Z".to_string(),
            }
        );
    }

    #[test]
    fn dedupes_consecutive_process_details() {
        let items = vec![
            ProcessDetailItem {
                id: "pd-1".to_string(),
                message_id: "msg-1".to_string(),
                conversation_id: "conv-1".to_string(),
                event_type: "progress".to_string(),
                message: "same".to_string(),
                data: json!({"n": 1}),
                created_at: "2026-06-26T00:00:00Z".to_string(),
            },
            ProcessDetailItem {
                id: "pd-2".to_string(),
                message_id: "msg-1".to_string(),
                conversation_id: "conv-1".to_string(),
                event_type: "progress".to_string(),
                message: "same".to_string(),
                data: json!({"n": 1}),
                created_at: "2026-06-26T00:00:01Z".to_string(),
            },
            ProcessDetailItem {
                id: "pd-3".to_string(),
                message_id: "msg-1".to_string(),
                conversation_id: "conv-1".to_string(),
                event_type: "progress".to_string(),
                message: "changed".to_string(),
                data: json!({"n": 1}),
                created_at: "2026-06-26T00:00:02Z".to_string(),
            },
        ];

        let got = dedupe_process_details(items);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "pd-1");
        assert_eq!(got[1].id, "pd-3");
    }

    #[test]
    fn normalizes_hitl_config() {
        let got = normalize_hitl_config(HitlConfig {
            enabled: true,
            mode: " feedback ".to_string(),
            sensitive_tools: vec![" nmap ".to_string(), "".to_string()],
            timeout_seconds: -5,
        });
        assert_eq!(
            got,
            HitlConfig {
                enabled: true,
                mode: "approval".to_string(),
                sensitive_tools: vec!["nmap".to_string()],
                timeout_seconds: 0,
            }
        );

        let off = normalize_hitl_config(HitlConfig {
            enabled: false,
            mode: "review_edit".to_string(),
            sensitive_tools: Vec::new(),
            timeout_seconds: 10,
        });
        assert_eq!(off.mode, "off");
    }

    #[test]
    fn normalizes_hitl_pending_query_bounds() {
        let q = ListHitlPendingQuery {
            conversation_id: Some(" conv ".to_string()),
            status: Some("all".to_string()),
            page: Some(0),
            page_size: Some(500),
        }
        .normalized();
        assert_eq!(
            q,
            NormalizedHitlPendingQuery {
                conversation_id: "conv".to_string(),
                status: "all".to_string(),
                status_all: true,
                page: 1,
                page_size: 200,
            }
        );
        assert_eq!(q.offset(), 0);
    }

    #[test]
    fn normalizes_hitl_interrupt_input() {
        let got = normalize_hitl_interrupt_input(UpsertHitlInterruptRequest {
            id: " hitl-1 ".to_string(),
            conversation_id: " conv-1 ".to_string(),
            message_id: Some(" msg-1 ".to_string()),
            mode: " feedback ".to_string(),
            tool_name: " nmap ".to_string(),
            tool_call_id: Some(" tool-1 ".to_string()),
            payload: Some("{\"cmd\":\"nmap\"}".to_string()),
            status: None,
        })
        .expect("normalize HITL interrupt");

        assert_eq!(
            got,
            NormalizedHitlInterruptInput {
                id: "hitl-1".to_string(),
                conversation_id: "conv-1".to_string(),
                message_id: "msg-1".to_string(),
                mode: "approval".to_string(),
                tool_name: "nmap".to_string(),
                tool_call_id: "tool-1".to_string(),
                payload: "{\"cmd\":\"nmap\"}".to_string(),
                status: "pending".to_string(),
            }
        );
    }

    #[test]
    fn maps_permission_reply_from_existing_decision_body() {
        assert_eq!(
            normalize_permission_reply(None, "approve").expect("approve"),
            PermissionReply::Once
        );
        assert_eq!(
            normalize_permission_reply(Some("always"), "approve").expect("always"),
            PermissionReply::Always
        );
        assert_eq!(
            normalize_permission_reply(Some("reject"), "").expect("reject"),
            PermissionReply::Reject
        );
    }

    #[test]
    fn permission_rules_use_last_matching_rule() {
        let mut state = PermissionState::default();
        state.session_rules.insert(
            "conv:sess".to_string(),
            vec![
                PermissionRule {
                    permission: "execute".to_string(),
                    pattern: "command:npm *".to_string(),
                    action: PermissionAction::Deny,
                },
                PermissionRule {
                    permission: "execute".to_string(),
                    pattern: "command:npm test".to_string(),
                    action: PermissionAction::Allow,
                },
            ],
        );
        assert_eq!(
            permission_rules_decide(
                &state,
                "conv:sess",
                "execute",
                &["command:npm test".to_string()],
                "execute",
            ),
            Some(PermissionAction::Allow)
        );
    }

    #[test]
    fn injects_permission_context_into_runtime_command() {
        let mut command = json!({"type": "start_turn", "context": {}});
        inject_permission_context(&mut command, "http://127.0.0.1:51283/", "msg-1");
        let ctx = command.get("context").and_then(Value::as_object).unwrap();
        assert_eq!(
            ctx.get("hitl_permission_ask_url").and_then(Value::as_str),
            Some("http://127.0.0.1:51283/api/internal/hitl/permission-ask")
        );
        assert_eq!(
            ctx.get("assistant_message_id").and_then(Value::as_str),
            Some("msg-1")
        );
    }
}
