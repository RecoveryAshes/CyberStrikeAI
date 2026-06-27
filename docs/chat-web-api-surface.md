# Chat Web API Surface

This document records the current API surface used by `apps/chat-web` when served from the Parrot preview URL `http://192.168.64.2:4177`.

The frontend talks only to Go HTTP/SSE APIs under `/api`. Vite preview proxies those requests to the Go backend on `http://192.168.64.2:51282`. The frontend does not directly connect to Rust, Redis, PostgreSQL, or SQLite.

## Current Runtime Shape

```text
Browser / apps/chat-web :4177
  -> Vite preview proxy
  -> Go HTTP/SSE backend :51282
      -> Rust Agent Runtime over gRPC
      -> Redis runtime state/events
      -> SQLite application data
```

PostgreSQL is available as an infrastructure target, but these endpoints currently remain served by the Go backend and the existing application database layer unless explicitly migrated.

## Migration Status Legend

- `go`: currently served by Go.
- `rust-planned`: selected for Rust implementation.
- `rust-shadow`: Rust implementation exists, Go still serves traffic.
- `rust-proxied`: Go proxies this path to Rust.
- `rust-direct`: frontend can target Rust directly for this path.

Use the `Done` checkbox only when the endpoint has completed the agreed migration target and has been validated against the chat-web contract. Until then, leave it unchecked and use `Migration status` to show the current phase.

## Config And Model

| Done | Method | Path | Current owner | Migration status | Frontend use |
| --- | --- | --- | --- | --- | --- |
| [ ] | `GET` | `/api/config` | Go | `go` | Load current app config, model, reasoning, and agent runtime settings. |
| [ ] | `PUT` | `/api/config` | Go | `go` | Update config. The composer Model control updates `openai.model` through this endpoint. |
| [ ] | `POST` | `/api/config/list-models` | Go | `go` | Fetch available model IDs from the configured provider. |

## Roles And Projects

| Done | Method | Path | Current owner | Migration status | Frontend use |
| --- | --- | --- | --- | --- | --- |
| [ ] | `GET` | `/api/roles` | Go | `go` | Load the Role dropdown. |
| [ ] | `GET` | `/api/projects?limit=500` | Go | `go` | Load the Project dropdown. |
| [ ] | `PUT` | `/api/conversations/:id/project` | Go | `go` | Bind an existing conversation to a project. |

## Conversations And Messages

| Done | Method | Path | Current owner | Migration status | Frontend use |
| --- | --- | --- | --- | --- | --- |
| [ ] | `GET` | `/api/conversations?limit=200&sort_by=updated_at...` | Go | `go` | Load the left sidebar conversation list. Optional `search` query is appended by the frontend. |
| [ ] | `POST` | `/api/conversations` | Go | `go` | Create a new conversation. |
| [ ] | `GET` | `/api/conversations/:id?include_process_details=0` | Go | `go` | Open conversation detail and messages. |
| [ ] | `PUT` | `/api/conversations/:id` | Go | `go` | Rename a conversation. |
| [ ] | `DELETE` | `/api/conversations/:id` | Go | `go` | Delete a conversation. |
| [ ] | `GET` | `/api/messages/:id/process-details` | Go | `go` | Load historical process details for assistant messages. |

## Agent Runtime And Streaming

| Done | Method | Path | Current owner | Migration status | Frontend use |
| --- | --- | --- | --- | --- | --- |
| [ ] | `POST` | `/api/agent-runtime/stream` | Go facade + Rust runtime | `go` | Send a message and start a background Rust Agent Runtime turn. |
| [ ] | `GET` | `/api/agent-loop/task-events` | Go SSE facade + Redis reader | `go` | Global `EventSource` for live task/runtime events. |
| [ ] | `GET` | `/api/agent-loop/tasks` | Go facade + Redis reader | `go` | Poll active task/run state. |
| [ ] | `POST` | `/api/agent-loop/cancel` | Go facade + Rust runtime | `go` | Cancel the active run for a conversation. |

## HITL

| Done | Method | Path | Current owner | Migration status | Frontend use |
| --- | --- | --- | --- | --- | --- |
| [ ] | `GET` | `/api/hitl/config/:conversationId` | Go | `go` | Load per-conversation HITL config. |
| [ ] | `PUT` | `/api/hitl/config` | Go | `go` | Save per-conversation HITL config. |
| [ ] | `GET` | `/api/hitl/pending?status=pending&pageSize=50...` | Go | `go` | Query pending approvals, optionally scoped by `conversationId`. |
| [ ] | `POST` | `/api/hitl/decision` | Go facade + Rust runtime resume | `go` | Approve or reject a pending HITL request. |

## Notes For Incremental Rust Migration

- Keep `apps/chat-web` pointed at one `/api` origin during migration.
- Prefer Go as the compatibility entry point while individual paths move to Rust.
- For each migrated endpoint, preserve the frontend JSON shape and status-code behavior.
- Avoid dual writes for the same domain object. A migrated module should have a single write owner.
- Add contract tests comparing Go and Rust responses before switching a path to `rust-proxied`.
