# CyberStrikeAI Rust API

Independent Rust HTTP API service used behind the existing Go `/api` facade.

Current migrated endpoints:

- `GET /api/config`
- `PUT /api/config`
- `POST /api/config/list-models`
- `GET /api/roles`
- `GET /api/projects`
- `POST /api/agent-runtime/stream`
- `GET /api/agent-loop/tasks`
- `GET /api/agent-loop/task-events`
- `POST /api/agent-loop/cancel`
- `GET /api/hitl/config/:conversationId`
- `PUT /api/hitl/config`
- `GET /api/hitl/pending`
- `POST /api/hitl/decision`

The config endpoints store only the 4177 `apps/chat-web` frontend OpenAI settings in PostgreSQL (`app_config`, key `chat_web_frontend`):

- `openai.provider`
- `openai.api_key`
- `openai.base_url`
- `openai.model`
- `openai.reasoning.effort`

They do not read from or write `config.yaml`, and they do not use SQLite.

The roles/projects endpoints expose 4177 frontend read models from PostgreSQL. Role and project write endpoints still belong to the Go backend until migrated separately.

The Agent Runtime endpoints own the 4177 chat-web runtime path:

- `POST /api/agent-runtime/stream` accepts the current frontend chat payload (`message`, optional `conversationId`, `projectId`, `role`, `reasoning`, `hitl`, `background`, `attachments`).
- Rust creates or validates the conversation, saves the user message, creates the assistant placeholder, builds the runtime command from PostgreSQL config/roles/history/HITL, starts the JSONL `agent-runtime` process, persists task state/events, finalizes the assistant message, and publishes title/task/HITL updates.
- Go is only the auth boundary and streaming proxy for `/api/agent-runtime/stream`, `/api/agent-loop/task-events`, `/api/agent-loop/tasks`, `/api/agent-loop/cancel`, and `/api/hitl/*`.

Rust owns Agent Runtime MCP discovery, schema budgeting, and builtin/local tool execution. The runtime command passes `role_tools`, `mcp_tools_dir`, budget knobs, and optional `external_mcp_endpoint_url`; `agent-runtime` reads local `tools/*.yaml`, filters the searchable/callable catalog by role allowlist, injects only a compact catalog into the system prompt, exposes `tool_search`, and sends full OpenAI-compatible function schemas only for tools loaded into the current session and admitted by the dynamic schema budget. Builtin/local YAML tools execute in Rust by mapping `command`, `args`, and parameter `flag`/`positional`/`template`/`combined` rules into a bounded local process. `external_mcp_endpoint_url`/`mcp_endpoint_url` is only an external MCP compatibility client path for non-builtin sources; it is not required for builtin/local tools and must not point to Go business logic as a dependency.

The MCP loaded state is persisted in Rust runtime session state under the workspace session store. Records include identity, state (`selected_pending`, `loaded`, `recently_used`, `budget_blocked`), selected/used timestamps, use count, and schema hash; records with stale schema hashes are ignored on restore.

Current Rust-owned TODOs are explicit runtime context gaps, not Go API dependencies: vector knowledge snippets are not yet fully materialized in PostgreSQL for Rust to load.

The HITL endpoints persist conversation config, pending approvals, and decisions in PostgreSQL. Rust owns Agent Runtime permission waiters; `/api/hitl/decision` wakes the in-process Rust waiter for the running task.

Default runtime:

```bash
DATABASE_URL=postgres://cyberstrike:cyberstrike@127.0.0.1:5432/cyberstrike \
RUSTAPI_LISTEN=127.0.0.1:51283 \
AGENT_RUNTIME_BINARY_PATH=/home/user/CyberStrikeAI/agent-runtime/target/release/cyberstrike-agent-runtime \
AGENT_RUNTIME_WORK_DIR=/home/user/CyberStrikeAI \
cargo run -- serve
```

The Go backend remains the frontend entrypoint and authentication boundary. Migrated config paths are proxied from Go to this service.
