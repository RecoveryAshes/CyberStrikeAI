use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use crate::cancellation::CancelToken;
use crate::event_protocol::{new_id, RuntimeCommand, RuntimeEvent};
use crate::session_store::{ActiveRunGuard, SessionStore, StoredSession};
use crate::turn_loop::TurnLoop;

#[derive(Debug, Clone, Default)]
pub struct SessionLoop {
    sessions: Arc<Mutex<HashMap<String, RuntimeSession>>>,
}

#[derive(Debug, Clone)]
struct RuntimeSession {
    id: String,
    active_turn_id: Option<String>,
    turn_count: u64,
    state_summary: String,
}

impl SessionLoop {
    #[cfg(test)]
    pub fn handle(&self, command: RuntimeCommand) -> Vec<RuntimeEvent> {
        self.handle_with_event_sink(command, &mut |_| {}, CancelToken::default())
    }

    pub fn handle_with_event_sink(
        &self,
        command: RuntimeCommand,
        on_event: &mut impl FnMut(RuntimeEvent),
        cancel_token: CancelToken,
    ) -> Vec<RuntimeEvent> {
        match command {
            RuntimeCommand::StartTurn {
                command_id: _,
                conversation_id,
                runtime_session_id,
                message,
                context,
            } => {
                let store = SessionStore::from_context(&context);
                let session_id = runtime_session_id
                    .filter(|id| !id.trim().is_empty())
                    .unwrap_or_else(|| new_id("session"));
                let stored_session = store
                    .load(&conversation_id, &session_id)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| {
                        StoredSession::new(conversation_id.clone(), session_id.clone())
                    });
                if stored_session.pending_approval.is_some() {
                    return emit_event_vec(
                        RuntimeEvent::runtime_error(
                            conversation_id,
                            session_id,
                            "conversation already has an active runtime submission",
                        ),
                        on_event,
                    );
                }
                let turn_id = new_id("turn");
                let active_guard =
                    match claim_active_run(&store, &conversation_id, &session_id, &turn_id) {
                        Ok(guard) => guard,
                        Err(err) => {
                            return emit_event_vec(
                                RuntimeEvent::runtime_error(
                                    conversation_id,
                                    session_id,
                                    format!("claim active runtime submission: {}", err),
                                ),
                                on_event,
                            );
                        }
                    };
                {
                    let mut sessions = self.sessions.lock().expect("session loop poisoned");
                    let session =
                        sessions
                            .entry(conversation_id.clone())
                            .or_insert_with(|| RuntimeSession {
                                id: session_id.clone(),
                                active_turn_id: None,
                                turn_count: stored_session.turn_count,
                                state_summary: stored_session.state_summary.clone(),
                            });
                    if session.id != session_id {
                        session.id = session_id.clone();
                        session.turn_count = stored_session.turn_count;
                        session.state_summary = stored_session.state_summary.clone();
                    }
                    session.active_turn_id = Some(turn_id.clone());
                    session.state_summary = "active".to_string();
                }

                let mut events = Vec::new();
                emit_event(
                    &mut events,
                    on_event,
                    RuntimeEvent::SessionStarted {
                        conversation_id: conversation_id.clone(),
                        runtime_session_id: session_id.clone(),
                    },
                );

                let mut stored_active = stored_session.clone();
                stored_active.mark_active(turn_id.clone());
                if let Err(err) = store.save(&stored_active) {
                    emit_event(
                        &mut events,
                        on_event,
                        RuntimeEvent::runtime_error(
                            &conversation_id,
                            &session_id,
                            format!("save active session state: {}", err),
                        ),
                    );
                }

                let turn_result =
                    TurnLoop::new(conversation_id.clone(), session_id.clone(), turn_id.clone())
                        .run_with_event_sink(message, context, on_event, cancel_token);
                if let Some(pending) = turn_result.pending_approval.clone() {
                    stored_active.mark_pending_approval(pending);
                    stored_active
                        .append_compaction_artifacts(turn_result.compaction_artifacts.clone());
                    if let Err(err) = store.save(&stored_active) {
                        emit_event(
                            &mut events,
                            on_event,
                            RuntimeEvent::runtime_error(
                                &conversation_id,
                                &session_id,
                                format!("save pending approval session state: {}", err),
                            ),
                        );
                    }
                }
                events.extend(turn_result.events);

                let state_summary = terminal_state_summary(&events).to_string();
                {
                    let mut sessions = self.sessions.lock().expect("session loop poisoned");
                    let session =
                        sessions
                            .entry(conversation_id.clone())
                            .or_insert_with(|| RuntimeSession {
                                id: session_id.clone(),
                                active_turn_id: None,
                                turn_count: stored_session.turn_count,
                                state_summary: stored_session.state_summary.clone(),
                            });
                    session.active_turn_id = None;
                    session.turn_count = session.turn_count.saturating_add(1);
                    session.state_summary = state_summary.clone();
                }

                let mut stored_finished = stored_active;
                let last_turn_id = terminal_turn_id(&events);
                if turn_result.pending_approval.is_some() {
                    stored_finished.state_summary = "pending_approval".to_string();
                } else if let Some(last_turn_id) = last_turn_id {
                    stored_finished.append_compaction_artifacts(turn_result.compaction_artifacts);
                    stored_finished.mark_finished(last_turn_id, state_summary);
                }
                if let Err(err) = store.save(&stored_finished) {
                    emit_event(
                        &mut events,
                        on_event,
                        RuntimeEvent::runtime_error(
                            &stored_finished.conversation_id,
                            &stored_finished.runtime_session_id,
                            format!("save finished session state: {}", err),
                        ),
                    );
                }
                drop(active_guard);
                events
            }
            RuntimeCommand::InterruptTurn {
                command_id: _,
                conversation_id,
                reason,
                continue_after: _,
            } => {
                let sessions = self.sessions.lock().expect("session loop poisoned");
                let session = sessions.get(&conversation_id);
                let runtime_session_id = session.map(|s| s.id.clone()).unwrap_or_default();
                let turn_id = session
                    .and_then(|s| s.active_turn_id.clone())
                    .unwrap_or_default();
                let event = RuntimeEvent::TurnAborted {
                    conversation_id,
                    runtime_session_id,
                    turn_id,
                    reason,
                };
                emit_event_vec(event, on_event)
            }
            RuntimeCommand::ApprovalResponse {
                command_id: _,
                conversation_id,
                runtime_session_id,
                request_id,
                decision,
                message,
                context,
            } => {
                let store = SessionStore::from_context(&context);
                let Some(session_id) = runtime_session_id
                    .filter(|id| !id.trim().is_empty())
                    .or_else(|| {
                        self.sessions
                            .lock()
                            .expect("session loop poisoned")
                            .get(&conversation_id)
                            .map(|session| session.id.clone())
                    })
                else {
                    return emit_event_vec(
                        RuntimeEvent::runtime_error(
                            conversation_id,
                            "",
                            "approval_response missing runtime_session_id",
                        ),
                        on_event,
                    );
                };
                let mut stored = match store.load(&conversation_id, &session_id) {
                    Ok(Some(stored)) => stored,
                    Ok(None) => {
                        return emit_event_vec(
                            RuntimeEvent::runtime_error(
                                conversation_id,
                                session_id,
                                "approval_response session not found",
                            ),
                            on_event,
                        );
                    }
                    Err(err) => {
                        return emit_event_vec(
                            RuntimeEvent::runtime_error(
                                conversation_id,
                                session_id,
                                format!("load approval session state: {}", err),
                            ),
                            on_event,
                        );
                    }
                };
                let Some(pending) = stored.pending_approval.clone() else {
                    return emit_event_vec(
                        RuntimeEvent::runtime_error(
                            conversation_id,
                            session_id,
                            "approval_response has no pending approval",
                        ),
                        on_event,
                    );
                };
                if pending.request_id != request_id {
                    return emit_event_vec(
                        RuntimeEvent::runtime_error(
                            conversation_id,
                            session_id,
                            format!(
                                "approval_response request_id mismatch: got {}, pending {}",
                                request_id, pending.request_id
                            ),
                        ),
                        on_event,
                    );
                }
                let turn_id = pending.turn_id.clone();
                let active_guard =
                    match claim_active_run(&store, &conversation_id, &session_id, &turn_id) {
                        Ok(guard) => guard,
                        Err(err) => {
                            return emit_event_vec(
                                RuntimeEvent::runtime_error(
                                    conversation_id,
                                    session_id,
                                    format!("claim active runtime submission: {}", err),
                                ),
                                on_event,
                            );
                        }
                    };
                {
                    let mut sessions = self.sessions.lock().expect("session loop poisoned");
                    sessions.insert(
                        conversation_id.clone(),
                        RuntimeSession {
                            id: session_id.clone(),
                            active_turn_id: Some(turn_id.clone()),
                            turn_count: stored.turn_count,
                            state_summary: "active".to_string(),
                        },
                    );
                }
                let turn_result =
                    TurnLoop::new(conversation_id.clone(), session_id.clone(), turn_id)
                        .resume_after_approval_with_event_sink(
                            pending,
                            decision,
                            message,
                            context,
                            on_event,
                            cancel_token,
                        );
                let events = turn_result.events;
                let state_summary = terminal_state_summary(&events).to_string();
                if let Some(pending) = turn_result.pending_approval.clone() {
                    stored.mark_pending_approval(pending);
                } else if let Some(last_turn_id) = terminal_turn_id(&events) {
                    stored.mark_finished(last_turn_id, state_summary);
                } else {
                    stored.pending_approval = None;
                }
                stored.append_compaction_artifacts(turn_result.compaction_artifacts);
                {
                    let mut sessions = self.sessions.lock().expect("session loop poisoned");
                    let session =
                        sessions
                            .entry(conversation_id.clone())
                            .or_insert_with(|| RuntimeSession {
                                id: session_id.clone(),
                                active_turn_id: None,
                                turn_count: stored.turn_count,
                                state_summary: stored.state_summary.clone(),
                            });
                    session.id = session_id.clone();
                    session.active_turn_id = stored.active_turn_id.clone();
                    session.turn_count = stored.turn_count;
                    session.state_summary = stored.state_summary.clone();
                }
                if let Err(err) = store.save(&stored) {
                    let mut with_error = events;
                    emit_event(
                        &mut with_error,
                        on_event,
                        RuntimeEvent::runtime_error(
                            conversation_id,
                            session_id,
                            format!("save approval response session state: {}", err),
                        ),
                    );
                    return with_error;
                }
                drop(active_guard);
                events
            }
            RuntimeCommand::Shutdown => Vec::new(),
        }
    }
}

fn emit_event(
    events: &mut Vec<RuntimeEvent>,
    on_event: &mut impl FnMut(RuntimeEvent),
    event: RuntimeEvent,
) {
    on_event(event.clone());
    events.push(event);
}

fn emit_event_vec(
    event: RuntimeEvent,
    on_event: &mut impl FnMut(RuntimeEvent),
) -> Vec<RuntimeEvent> {
    on_event(event.clone());
    vec![event]
}

fn terminal_state_summary(events: &[RuntimeEvent]) -> &'static str {
    if events
        .iter()
        .any(|event| matches!(event, RuntimeEvent::TurnCompleted { .. }))
    {
        "completed"
    } else if events
        .iter()
        .any(|event| matches!(event, RuntimeEvent::ApprovalRequested { .. }))
    {
        "pending_approval"
    } else if events
        .iter()
        .any(|event| matches!(event, RuntimeEvent::TurnAborted { .. }))
    {
        "aborted"
    } else if events
        .iter()
        .any(|event| matches!(event, RuntimeEvent::RuntimeError { .. }))
    {
        "error"
    } else {
        "stopped"
    }
}

fn terminal_turn_id(events: &[RuntimeEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| match event {
        RuntimeEvent::TurnCompleted { turn_id, .. }
        | RuntimeEvent::TurnAborted { turn_id, .. }
        | RuntimeEvent::StopHookContinued { turn_id, .. }
        | RuntimeEvent::CompactionCompleted { turn_id, .. }
        | RuntimeEvent::FollowUpStarted { turn_id, .. }
        | RuntimeEvent::TurnStarted { turn_id, .. } => Some(turn_id.clone()),
        _ => None,
    })
}

fn claim_active_run(
    store: &SessionStore,
    conversation_id: &str,
    runtime_session_id: &str,
    turn_id: &str,
) -> io::Result<ActiveRunGuard> {
    store.claim_active_run(conversation_id, runtime_session_id, turn_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map, Value};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn persists_session_state_between_loop_instances() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-session-loop-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut context = Map::new();
        context.insert(
            "session_store_dir".to_string(),
            json!(root.to_string_lossy().to_string()),
        );

        let first = SessionLoop::default();
        let events = first.handle(RuntimeCommand::StartTurn {
            command_id: String::new(),
            conversation_id: "conv-1".to_string(),
            runtime_session_id: Some("session-1".to_string()),
            message: "hello".to_string(),
            context: context.clone(),
        });
        assert!(events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnCompleted { .. })));

        let second = SessionLoop::default();
        let events = second.handle(RuntimeCommand::StartTurn {
            command_id: String::new(),
            conversation_id: "conv-1".to_string(),
            runtime_session_id: Some("session-1".to_string()),
            message: "hello again".to_string(),
            context,
        });
        assert!(events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnCompleted { .. })));

        let store = SessionStore::from_context(&{
            let mut ctx = Map::new();
            ctx.insert(
                "session_store_dir".to_string(),
                json!(root.to_string_lossy().to_string()),
            );
            ctx
        });
        let stored = store.load("conv-1", "session-1").unwrap().unwrap();
        assert_eq!(stored.turn_count, 2);
        assert_eq!(stored.state_summary, "completed");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn approval_response_resumes_pending_tool_and_completes_turn() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-session-approval-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let endpoint = start_mock_mcp_server(
            r#"{"jsonrpc":"2.0","id":"cyberstrike-agent-runtime-mcp-call","result":{"content":[{"type":"text","text":"approved lookup result"}]}}"#,
        );
        let mut context = Map::new();
        context.insert(
            "session_store_dir".to_string(),
            json!(root.to_string_lossy().to_string()),
        );
        context.insert("simulate_mcp".to_string(), Value::Bool(true));
        context.insert("approval_enabled".to_string(), Value::Bool(true));
        context.insert("mcp_enabled".to_string(), Value::Bool(true));
        context.insert("mcp_endpoint_url".to_string(), Value::String(endpoint));
        context.insert(
            "openai_api_key".to_string(),
            Value::String("sk-test-secret".to_string()),
        );
        context.insert(
            "mcp_auth_header".to_string(),
            Value::String("X-MCP-Token".to_string()),
        );
        context.insert(
            "mcp_auth_header_value".to_string(),
            Value::String("mcp-test-secret".to_string()),
        );
        context.insert(
            "mcp_tools".to_string(),
            json!([
                {"server": "demo", "name": "lookup", "enabled": true, "requires_approval": true}
            ]),
        );

        let loop_state = SessionLoop::default();
        let events = loop_state.handle(RuntimeCommand::StartTurn {
            command_id: String::new(),
            conversation_id: "conv-approval".to_string(),
            runtime_session_id: Some("session-approval".to_string()),
            message: "lookup runtime".to_string(),
            context: context.clone(),
        });
        let request_id = events
            .iter()
            .find_map(|event| match event {
                RuntimeEvent::ApprovalRequested { request_id, .. } => Some(request_id.clone()),
                _ => None,
            })
            .unwrap();
        assert!(!events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnCompleted { .. })));

        let session_path = root.join("conv-approval").join("session-approval.json");
        let raw_session = std::fs::read_to_string(&session_path).unwrap();
        assert!(raw_session.contains("sk-test-secret"));
        assert!(raw_session.contains("mcp-test-secret"));
        assert!(raw_session.contains("openai_api_key"));
        assert!(raw_session.contains("mcp_auth_header_value"));
        assert!(raw_session.contains("mcp_auth_header"));

        let events = loop_state.handle(RuntimeCommand::ApprovalResponse {
            command_id: String::new(),
            conversation_id: "conv-approval".to_string(),
            runtime_session_id: Some("session-approval".to_string()),
            request_id,
            decision: "approve".to_string(),
            message: String::new(),
            context: context.clone(),
        });
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallCompleted { result, .. } if result.contains("approved lookup result")
        )));
        assert!(events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnCompleted { .. })));

        let store = SessionStore::from_context(&context);
        let stored = store
            .load("conv-approval", "session-approval")
            .unwrap()
            .unwrap();
        assert!(stored.pending_approval.is_none());
        assert_eq!(stored.state_summary, "completed");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stored_pending_approval_blocks_new_start_turn() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-session-active-pending-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut context = Map::new();
        context.insert(
            "session_store_dir".to_string(),
            json!(root.to_string_lossy().to_string()),
        );
        context.insert("simulate_mcp".to_string(), Value::Bool(true));
        context.insert("approval_enabled".to_string(), Value::Bool(true));
        context.insert("mcp_enabled".to_string(), Value::Bool(true));
        context.insert(
            "mcp_tools".to_string(),
            json!([
                {"server": "demo", "name": "lookup", "enabled": true, "requires_approval": true}
            ]),
        );

        let first = SessionLoop::default();
        let events = first.handle(RuntimeCommand::StartTurn {
            command_id: String::new(),
            conversation_id: "conv-pending".to_string(),
            runtime_session_id: Some("session-pending".to_string()),
            message: "lookup runtime".to_string(),
            context: context.clone(),
        });
        assert!(events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ApprovalRequested { .. })));

        let second = SessionLoop::default();
        let events = second.handle(RuntimeCommand::StartTurn {
            command_id: String::new(),
            conversation_id: "conv-pending".to_string(),
            runtime_session_id: Some("session-pending".to_string()),
            message: "new turn".to_string(),
            context: context.clone(),
        });

        assert!(matches!(
            events.as_slice(),
            [RuntimeEvent::RuntimeError { message, .. }] if message.contains("already has an active")
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_stored_active_turn_does_not_block_after_lock_is_released() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-session-stale-active-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut context = Map::new();
        context.insert(
            "session_store_dir".to_string(),
            json!(root.to_string_lossy().to_string()),
        );
        let store = SessionStore::from_context(&context);
        let mut stored = StoredSession::new("conv-stale".to_string(), "session-stale".to_string());
        stored.mark_active("turn-crashed".to_string());
        store.save(&stored).unwrap();

        let loop_state = SessionLoop::default();
        let events = loop_state.handle(RuntimeCommand::StartTurn {
            command_id: String::new(),
            conversation_id: "conv-stale".to_string(),
            runtime_session_id: Some("session-stale".to_string()),
            message: "recover stale turn".to_string(),
            context: context.clone(),
        });

        assert!(events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnCompleted { .. })));
        assert!(!events.iter().any(|event| matches!(
            event,
            RuntimeEvent::RuntimeError { message, .. } if message.contains("already has an active")
        )));
        let completed_turn_id = events
            .iter()
            .find_map(|event| match event {
                RuntimeEvent::TurnCompleted { turn_id, .. } => Some(turn_id.clone()),
                _ => None,
            })
            .expect("turn completed");
        let stored = store.load("conv-stale", "session-stale").unwrap().unwrap();
        assert_eq!(
            stored.last_turn_id.as_deref(),
            Some(completed_turn_id.as_str())
        );
        assert_ne!(stored.last_turn_id.as_deref(), Some("turn-crashed"));
        assert_eq!(stored.active_turn_id, None);
        assert_eq!(stored.state_summary, "completed");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn compaction_artifact_reference_is_persisted_in_session() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-session-loop-compaction-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut context = Map::new();
        context.insert(
            "session_store_dir".to_string(),
            json!(root.to_string_lossy().to_string()),
        );
        context.insert("compaction_enabled".to_string(), Value::Bool(true));
        context.insert("compaction_threshold_chars".to_string(), json!(10));
        context.insert("compaction_keep_recent_messages".to_string(), json!(1));
        context.insert("compaction_max_per_turn".to_string(), json!(1));

        let loop_state = SessionLoop::default();
        let events = loop_state.handle(RuntimeCommand::StartTurn {
            command_id: String::new(),
            conversation_id: "conv-compact".to_string(),
            runtime_session_id: Some("session-compact".to_string()),
            message: "This message is long enough to trigger compaction.".to_string(),
            context: context.clone(),
        });
        let artifact_path = events
            .iter()
            .find_map(|event| match event {
                RuntimeEvent::CompactionCompleted { artifact_path, .. } => {
                    Some(artifact_path.clone())
                }
                _ => None,
            })
            .expect("compaction completed event");
        assert!(!artifact_path.is_empty());
        let artifact = std::fs::read_to_string(&artifact_path).unwrap();
        assert!(artifact.contains("\"input_messages\""));
        assert!(artifact.contains("\"replacement_messages\""));

        let store = SessionStore::from_context(&context);
        let stored = store
            .load("conv-compact", "session-compact")
            .unwrap()
            .unwrap();
        assert_eq!(stored.compaction_artifacts.len(), 1);
        assert_eq!(stored.compaction_artifacts[0].path, artifact_path);
        assert_eq!(stored.compaction_tasks.len(), 1);
        assert_eq!(stored.compaction_tasks[0].status, "completed");
        assert_eq!(stored.compaction_tasks[0].artifact_path, artifact_path);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn start_mock_mcp_server(response_body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 8192];
            let _ = stream.read(&mut buffer).unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        format!("http://{}/mcp", addr)
    }
}
