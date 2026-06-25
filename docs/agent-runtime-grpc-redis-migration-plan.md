# Agent Runtime gRPC + Redis 迁移落地方案

## 结论

本次迁移只处理 `Agent Runtime` 与新前端对应的运行链路：

```text
apps/chat-web
  -> Go HTTP/SSE API
  -> Go Agent Runtime adapter
  -> Rust agent-runtime
```

目标不是把前端改成 gRPC，也不是重写整套 Go 后端。gRPC 只替换 Go 与 Rust 之间当前的 `PersistentClient + JSONL stdin/stdout` 边界。新前端继续使用 Go 暴露的 HTTP/SSE、任务、HITL、会话接口。

Redis 与 gRPC 一起进入本次迁移。Redis 只负责 Agent Runtime 的短期运行态：运行锁、状态、取消、审批索引、事件补流。SQLite 继续负责业务历史：conversation、message、process detail、HITL 审计、batch/task 历史。

## 当前代码事实

### Rust 当前范围

Rust 现在集中在 `agent-runtime/`，核心是独立 Agent Runtime：

- `agent-runtime/src/main.rs`
  - 读取 JSONL command。
  - 输出 JSONL event。
- `agent-runtime/src/event_protocol.rs`
  - 定义 `RuntimeCommand` 与 `RuntimeEvent`。
- `agent-runtime/src/submission_loop.rs`
  - 管同会话 active submission。
  - 管 cancellation registry。
- `agent-runtime/src/session_loop.rs`
  - 管 runtime session、turn、pending approval resume。
- `agent-runtime/src/session_store.rs`
  - 文件 session。
  - active run file lock。
  - pending approval 与 compaction artifact。
- `agent-runtime/src/turn_loop.rs`
  - 执行模型流、工具调用、审批、compaction、最终 turn result。
- `agent-runtime/src/tool_registry.rs`
  - 注册 MCP、skill、knowledge、filesystem 等 runtime tool。
- `agent-runtime/src/mcp_bridge.rs`
  - Rust 调 Go MCP HTTP endpoint。
- `agent-runtime/src/skill_runtime.rs`
  - Rust 侧 skill runtime 能力。
- `agent-runtime/src/knowledge_runtime.rs`
  - 消费 Go 注入的 knowledge snippets。
- `agent-runtime/src/filesystem_runtime.rs`
  - filesystem/shell runtime tools。

Rust 当前不是完整后端。它只覆盖 Agent Runtime 执行。

### Go 当前范围

Agent Runtime 相关 Go 代码主要在：

- `internal/agentruntime/client.go`
  - `PersistentClient` 启动 Rust 子进程。
  - stdin/stdout JSONL command/event。
  - 按 `conversation_id` 分发 event。
  - 处理 context cancel 后补发 `interrupt_turn`。
- `internal/handler/agent_runtime.go`
  - `/api/agent-runtime/stream` SSE handler。
  - 后台运行与 `TaskEventBus` 镜像。
  - SQLite `agent_runtime_sessions` 更新。
  - assistant message finalize。
  - HITL pending interrupt 创建。
  - Rust event 转成前端 SSE event。
  - 构造 Agent Runtime context：OpenAI、workspace、MCP tools、skills、knowledge snippets、approval、compaction。
- `internal/handler/task_manager.go`
  - Go 内存任务状态。
  - `/api/agent-loop/cancel` 取消入口依赖这里保存的 cancel func。
- `internal/handler/task_event_bus.go`
  - Go 内存 SSE 镜像，用于后台 run、刷新后补流、HITL resume 后继续推送。
- `internal/handler/hitl.go`
  - HITL 配置、pending 列表、审批决策、审计记录。
- `internal/database/agent_runtime.go`
  - SQLite `agent_runtime_sessions`。

### 新前端当前依赖

新前端在 `apps/chat-web/`，不直接接触 Rust：

- `apps/chat-web/src/api/client.ts`
  - `apiStream` 使用 HTTP `fetch` 读取 SSE。
- `apps/chat-web/src/components/workbench/ChatWorkbench.tsx`
  - `POST /api/agent-runtime/stream` 发起 Agent Runtime。
  - `GET /api/agent-loop/task-events` 订阅后台 task SSE 镜像。
  - `GET /api/agent-loop/tasks` 轮询运行中任务。
  - `/api/hitl/*` 管 HITL 配置、pending、审批。
- `apps/chat-web/src/runtime/eventAdapter.ts`
  - 解析 `response_delta`、`planning`、`tool_call`、`tool_result`、`tool_result_delta`、`hitl_approval_requested`、`done` 等 SSE event。
  - 依赖 `data.runtimeEventType` 与 `data.runtimeTrace`。
- `apps/chat-web/src/runtime/reducer.ts`
  - 管前端 run 状态：`running`、`awaiting_approval`、`completed`、`cancelled`、`error`。

因此前端协议当前保持 HTTP/SSE，不改成 gRPC。

## 边界决策

### 现在迁 Go -> Rust

#### 1. PersistentClient 运行态通信

从 Go 下沉到 Rust/gRPC：

- Rust 进程不再只读 stdin JSONL。
- Rust 增加 gRPC server。
- Go `PersistentClient` 改成 gRPC client facade。
- Go handler 调用方式保持 `StartTurn`、`InterruptTurn`、`ApprovalResponse` 语义不变。

当前 Go 中这部分应被替换：

- `PersistentClient.ensureStarted`
- `PersistentClient.writeCommand`
- `PersistentClient.registerRun`
- `PersistentClient.dispatchEvent`
- `scanPersistentStdout`
- `waitPersistentProcess`

Go 可以保留 runtime process bootstrap，但不再拥有 JSONL 事件分发。

#### 2. active run / cancellation / pending approval 运行态

从 Go 内存与 Rust 文件锁，迁到 Rust + Redis 为主：

- Rust 在 `StartTurn` 抢 Redis 运行锁。
- Rust 写 Redis run state。
- Rust 监听或读取 Redis cancel signal。
- Rust 写 pending approval index。
- Go 的 task list/cancel 变成 Redis/gRPC facade。

Go 不再把 Agent Runtime 的运行态只放在 `AgentTaskManager` 内存中。

#### 3. approval resume 的 session 定位

当前 Go 在 `resumeAgentRuntimeApproval` 里：

1. 查 `hitl_interrupts`。
2. 查 SQLite `agent_runtime_sessions`。
3. 拿 `runtime_session_id`。
4. 给 Rust 发 `approval_response`。

迁移后：

1. Go 继续查 `hitl_interrupts` 并记录审批结果。
2. Go 给 Rust/gRPC 发送 `request_id + decision + comment`。
3. Rust 根据 Redis `approval:{request_id}` 找 conversation/session/turn。
4. Rust 恢复 pending approval。

这样 runtime session 恢复逻辑回到 Rust。

#### 4. runtime event 标准结构

当前 Go 在 `agentRuntimeEventData` 与 `agentRuntimeTraceData` 里组装前端需要的 trace。迁移后 Rust gRPC event 应直接包含标准 envelope：

- `event_type`
- `conversation_id`
- `runtime_session_id`
- `turn_id`
- `assistant_message_id`
- `runtime_event_type`
- `runtime_trace_json`
- `payload_json`
- `occurred_at`
- `sequence`

Go 继续负责把 envelope 包成 SSE：

```json
{
  "type": "response_delta",
  "message": "...",
  "data": {
    "source": "agent_runtime",
    "runtimeEventType": "assistant_delta",
    "runtimeTrace": {}
  }
}
```

前端 `eventAdapter.ts` 不需要在第一阶段改协议。

#### 5. skill 加载

当前 Go `agentRuntimeSkills()` 扫描 skill 并把完整内容塞进 context。Rust 已有 `skill_runtime.rs`，这块应该迁到 Rust：

- Go context 只传 `skills_enabled`、`skills_dir`、role allowlist。
- Rust 读取 skill package。
- Rust 负责 skill content、package files、runtime lookup。

这样后续全 Rust 化时不需要再搬一次。

### 现在留在 Go

#### 1. HTTP/SSE handler

保留：

- `/api/agent-runtime/stream`
- `/api/codex-agent/stream`
- `/api/agent-loop/task-events`
- `/api/agent-loop/tasks`
- `/api/agent-loop/cancel`

原因：

- 新前端已经依赖这些 HTTP/SSE 接口。
- 本次改造目标是内部 transport 稳定，不是前端协议迁移。

#### 2. SQLite 业务历史

保留：

- conversation/message。
- assistant message finalize。
- process details。
- `agent_runtime_sessions` 作为 UI/历史兼容记录。
- batch task 历史。

Redis 不替代 SQLite。

#### 3. HITL API 与审计

保留：

- `/api/hitl/config`
- `/api/hitl/pending`
- `/api/hitl/decision`
- `hitl_interrupts` 审计记录。

Rust 只负责 runtime 暂停和恢复；Go 继续负责用户审批 API 与审计。

#### 4. MCP 管理

保留：

- Go builtin MCP server。
- external MCP manager。
- MCP 配置与工具启停。

Rust 继续通过 `mcp_bridge.rs` 调 Go MCP endpoint。MCP 服务端迁 Rust 是后续完整后端 Rust 化阶段的事。

#### 5. knowledge 检索

保留：

- Go `knowledgeRetriever`。
- SQLite fallback。

Rust 当前消费 snippets，不直接查询知识库。

## Redis 设计

Redis key 统一前缀：

```text
csai:agent_runtime:
```

### 运行锁

```text
csai:agent_runtime:run_lock:{conversation_id}
```

值：

```json
{
  "conversation_id": "conv",
  "runtime_session_id": "session",
  "turn_id": "turn",
  "owner": "runtime-instance-id",
  "started_at": "2026-06-24T00:00:00Z",
  "heartbeat_at": "2026-06-24T00:00:05Z"
}
```

规则：

- `StartTurn` 使用 `SET key value NX EX ttl`。
- 默认 TTL：7200 秒。
- Rust 每 10 秒续租。
- turn terminal 后删除 lock。
- lock 冲突返回标准 `runtime_error`，Go 转成当前前端可识别 error。

### 运行状态

```text
csai:agent_runtime:state:{conversation_id}
```

值：

```json
{
  "conversation_id": "conv",
  "runtime_session_id": "session",
  "turn_id": "turn",
  "status": "running",
  "message": "requesting model sample",
  "assistant_message_id": "msg",
  "updated_at": "2026-06-24T00:00:00Z"
}
```

状态枚举：

- `running`
- `awaiting_approval`
- `cancelling`
- `completed`
- `failed`
- `cancelled`

Go `/api/agent-loop/tasks` 在 Agent Runtime 模式下读取 Redis state，并与 Go 内存 `AgentTaskManager` 做兼容合并。第一阶段允许 Go 内存继续存在，但 Redis 是 Agent Runtime 的源头状态。

### 取消信号

```text
csai:agent_runtime:cancel:{conversation_id}
```

值：

```json
{
  "reason": "agent task cancelled by user",
  "continue_after": false,
  "requested_at": "2026-06-24T00:00:00Z"
}
```

规则：

- `/api/agent-loop/cancel` 写 Redis cancel key，并通过 gRPC `InterruptTurn` 通知 Rust。
- Rust turn loop 定期检查 cancel key。
- gRPC stream 断开时，Go 也写 cancel key。
- Rust 取消成功后写 `cancelled` state 并删除 cancel key。

### 审批索引

```text
csai:agent_runtime:approval:{request_id}
```

值：

```json
{
  "request_id": "approval_xxx",
  "conversation_id": "conv",
  "runtime_session_id": "session",
  "turn_id": "turn",
  "tool_call_id": "call",
  "tool_name": "builtin::tool",
  "assistant_message_id": "msg",
  "created_at": "2026-06-24T00:00:00Z"
}
```

规则：

- Rust 发 `approval_requested` 前写 approval index。
- Go 收到 event 后继续创建 `hitl_interrupts` pending 行。
- Go 审批决策后调用 Rust/gRPC `ApprovalResponse`，只传 `request_id + decision + comment`。
- Rust 根据 Redis index 找 session 并恢复。
- 审批 terminal 后删除 index。

### 事件补流

```text
csai:agent_runtime:events:{conversation_id}
```

使用 Redis Stream。

字段：

```text
event_type
conversation_id
runtime_session_id
turn_id
assistant_message_id
runtime_event_type
message
payload_json
runtime_trace_json
sequence
created_at
```

规则：

- Rust 写入每个 runtime event。
- Go 当前 SSE 连接直接转发 gRPC event。
- `TaskEventBus` 继续服务当前前端，但事件源改为 gRPC/Redis event。
- `/api/agent-loop/task-events` 断线重连时可从 Redis Stream 补最近事件。
- Stream 保留策略：按长度裁剪，默认每 conversation 保留 1000 条；terminal 后保留 24 小时。

## gRPC 接口

第一版 proto 放在：

```text
proto/agent_runtime/v1/agent_runtime.proto
```

核心接口：

```proto
service AgentRuntimeService {
  rpc Run(stream RuntimeCommand) returns (stream RuntimeEvent);
  rpc InterruptTurn(InterruptTurnRequest) returns (InterruptTurnResponse);
  rpc ResumeApproval(ResumeApprovalRequest) returns (stream RuntimeEvent);
  rpc GetRunState(GetRunStateRequest) returns (GetRunStateResponse);
  rpc ListRunStates(ListRunStatesRequest) returns (ListRunStatesResponse);
  rpc Health(HealthRequest) returns (HealthResponse);
}
```

`Run` 用 bidirectional stream，覆盖原 `start_turn` + event stream。

`ResumeApproval` 独立成 server stream，避免审批恢复与普通 start_turn 混在同一个 Go call path 里。

动态 JSON 字段第一版用 string 承载：

```proto
message JsonPayload {
  string json = 1;
}
```

原因：

- 当前 MCP arguments、tool result、context、runtimeTrace 都是动态 JSON。
- Go `map[string]interface{}` 与 Rust `serde_json::Value` 双端更容易保持原样。
- 第一版减少 protobuf Struct 的类型损耗和生成代码复杂度。

## Go 适配层改造

### 保留 handler 函数名

保留以下函数入口，内部切到 gRPC：

- `AgentRuntimeLoopStream`
- `executeAgentRuntimeStreamTurn`
- `runAgentRuntimeTurn`
- `resumeAgentRuntimeApproval`
- `handleAgentRuntimeEvent`

这样新前端、机器人、batch agent_runtime 模式不改调用入口。

### 替换 PersistentClient

新增接口：

```go
type RuntimeClient interface {
    StartTurn(ctx context.Context, cmd Command, onEvent func(Event) error) error
    InterruptTurn(ctx context.Context, conversationID, reason string, continueAfter bool) error
    ResumeApproval(ctx context.Context, req ApprovalResponse, onEvent func(Event) error) error
    ListRunStates(ctx context.Context) ([]RunState, error)
    Close() error
}
```

实现：

- `JSONLPersistentClient`
  - 包装当前实现。
  - 用于 rollback。
- `GRPCRuntimeClient`
  - 新实现。
  - 默认目标。

配置开关：

```yaml
agent_runtime:
  enabled: true
  transport: grpc # jsonl | grpc
  grpc_listen: 127.0.0.1:0
  redis_addr: 127.0.0.1:6379
  redis_prefix: "csai:agent_runtime:"
```

### SSE 兼容层

Go 继续输出前端已消费的 SSE 类型：

- `conversation`
- `message_saved`
- `progress`
- `runtime_status_update`
- `planning`
- `reasoning_chain_stream_delta`
- `response_delta`
- `tool_call`
- `tool_result_delta`
- `tool_result`
- `hitl_approval_requested`
- `hitl_approval_resolved`
- `response`
- `cancelled`
- `error`
- `done`

第一阶段不改 `apps/chat-web/src/runtime/eventAdapter.ts` 的协议假设。

## Rust 改造

新增文件：

```text
agent-runtime/build.rs
agent-runtime/src/grpc_server.rs
agent-runtime/src/grpc_protocol.rs
agent-runtime/src/redis_state.rs
```

`main.rs` 支持 transport 参数：

```text
cyberstrike-agent-runtime --transport jsonl
cyberstrike-agent-runtime --transport grpc --listen 127.0.0.1:0 --redis-addr 127.0.0.1:6379
```

Rust 内部仍复用：

- `SubmissionLoop`
- `SessionLoop`
- `TurnLoop`
- `CancellationRegistry`
- `SessionStore`

新增 Redis state adapter 后，`SessionLoop` 和 `SubmissionLoop` 不直接散落 Redis 代码。Redis 操作集中在 `redis_state.rs`。

## 迁移阶段

### P0 文档与协议冻结

输出物：

- 本文档。
- `proto/agent_runtime/v1/agent_runtime.proto` 草案。
- 当前 SSE event compatibility checklist。

退出标准：

- 所有当前前端依赖 event 类型列入 checklist。
- Go/Rust 字段映射覆盖 `event_protocol.rs`。

### P1 Rust gRPC server 并行 JSONL

改动：

- Rust 增加 gRPC server。
- JSONL 入口保留。
- gRPC 调用复用 `SubmissionLoop`。

退出标准：

- `cargo test --manifest-path agent-runtime/Cargo.toml` 通过。
- gRPC start_turn 可以输出与 JSONL 等价事件序列。

回滚：

- `agent_runtime.transport=jsonl`。

### P2 Go gRPC client facade

改动：

- `internal/agentruntime` 增加 `RuntimeClient` interface。
- 当前 `PersistentClient` 包成 JSONL 实现。
- 增加 gRPC 实现。
- `AgentHandler.agentRuntimeClient()` 按配置选择。

退出标准：

- Go handler 测试覆盖 gRPC mocked event。
- `/api/agent-runtime/stream` SSE event 字段不变。

回滚：

- 配置切回 `jsonl`。

### P3 Redis runtime state

改动：

- Rust 写 run lock、state、cancel、approval index、event stream。
- Go `/api/agent-loop/tasks` 读取 Redis run state。
- Go `/api/agent-loop/cancel` 写 Redis cancel，并调用 gRPC interrupt。

退出标准：

- 同 conversation 并发只允许一个 run。
- Go 进程重启后能看到 Redis 中运行状态。
- 取消不依赖 Go 内存 cancel func。

回滚：

- `agent_runtime.redis_enabled=false`。
- 继续使用 Go `AgentTaskManager` 与 JSONL/gRPC interrupt。

### P4 approval resume 下沉 Rust

改动：

- Rust `approval_requested` 写 Redis approval index。
- Go `resumeAgentRuntimeApproval` 不再查 SQLite runtime session。
- Go 只传 `request_id + decision + comment` 给 Rust。

退出标准：

- 前端审批通过后，Rust 能恢复 pending approval。
- `hitl_interrupts` 仍保留完整审计。
- `hitl_approval_resolved`、后续 `response_delta`、`done` 正常通过 task events 推给前端。

回滚：

- 保留旧 SQLite session lookup 分支。

### P5 skill 加载下沉 Rust

改动：

- Go 不再把完整 skill content 注入 context。
- Go 只传 `skills_dir`、`skills_enabled`、role allowlist。
- Rust 扫描并加载 skill package。

退出标准：

- Rust runtime skill tools 行为与 Go 注入前一致。
- skill package file list 限制、路径边界、错误处理有测试。

回滚：

- 配置 `agent_runtime.skills_source=go_context`。

## 风险

### R1 前端 SSE 字段丢失

风险：

- 新前端依赖 `runtimeEventType`、`runtimeTrace`、`toolCallId`、`items`、`success` 等字段。

控制：

- P0 建 SSE compatibility fixture。
- P2 对 gRPC mock event 做 handler 测试。
- 第一阶段不改前端 eventAdapter。

### R2 Redis lock 泄露

风险：

- Rust crash 后 lock 未删除，同 conversation 无法继续。

控制：

- lock 必须有 TTL。
- heartbeat 续租。
- Go error path 不能直接无 TTL 写 lock。
- 新 run 遇到过期 lock 后可抢占。

### R3 取消重复或丢失

风险：

- Go context cancel、gRPC Interrupt、Redis cancel 同时存在。

控制：

- Rust 以 cancel token 幂等处理。
- cancel key 只表达目标状态。
- terminal event 后清理 cancel key。

### R4 approval resume 找不到 session

风险：

- Redis approval index 过期或 Rust session 文件丢失。

控制：

- approval index TTL 要大于 HITL 超时。
- Rust 返回明确 `runtime_error`。
- Go 保留旧 SQLite session lookup 作为 P4 回滚分支。

### R5 Redis 事件流与 SQLite process detail 不一致

风险：

- Redis 用于补流，SQLite 用于历史，两边可能顺序或字段不一致。

控制：

- Rust event 加 `sequence`。
- Go 写 process detail 时保留 `runtimeTrace`。
- terminal 后以前端历史展示仍以 SQLite 为准。

### R6 技能加载迁移导致工具缺失

风险：

- Go 与 Rust 对 skill package 的扫描规则不完全一致。

控制：

- P5 单独迁移。
- 保留 `skills_source=go_context` 回滚。
- 增加 skill package fixture 测试。

## 不做事项

本次不做：

- PostgreSQL。
- RabbitMQ。
- ClickHouse。
- JS 资产表。
- API endpoint 表。
- analysis run 表。
- evidence 表。
- 前端直连 gRPC。
- 全站 Go handler 重写 Rust。
- MCP 服务端整体迁 Rust。
- knowledge DB 迁 Rust。

这些事项不属于当前 Agent Runtime + 新前端对应链路。

## 最终全 Rust 路线

当前阶段完成后，系统形态是：

```text
新前端 -> Go HTTP/SSE facade -> Rust Agent Runtime gRPC -> Redis runtime state
                              -> SQLite business history
```

后续全 Rust 化时，迁移顺序应是：

1. Rust 接管 Agent Runtime HTTP/SSE facade。
2. Rust 接管 `/api/agent-loop/tasks`、`/task-events`、`/cancel`。
3. Rust 接管 HITL API，但保留原 SQLite 表结构或做明确数据迁移。
4. Rust 接管 conversation/message/process detail API。
5. Rust 接管 MCP 管理。
6. Rust 接管 knowledge 检索。
7. 下线 Go facade。

当前先完成 1 条内部边界：`Go facade -> Rust Agent Runtime gRPC`。这条边界稳定后，再推进外层 API Rust 化。
