use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::cancellation::{CancelToken, CancellationRegistry};
use crate::event_protocol::{RuntimeCommand, RuntimeEvent};
use crate::session_loop::SessionLoop;

#[derive(Debug, Clone, Default)]
pub struct SubmissionLoop {
    session_loop: SessionLoop,
    cancellations: CancellationRegistry,
    active_conversations: Arc<Mutex<HashSet<String>>>,
}

impl SubmissionLoop {
    pub fn with_cancellations(cancellations: CancellationRegistry) -> Self {
        Self {
            session_loop: SessionLoop::default(),
            cancellations,
            active_conversations: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    #[cfg(test)]
    pub fn handle(&self, command: RuntimeCommand) -> Vec<RuntimeEvent> {
        self.handle_with_event_sink(command, &mut |_| {})
    }

    pub fn handle_with_event_sink(
        &self,
        command: RuntimeCommand,
        on_event: &mut impl FnMut(RuntimeEvent),
    ) -> Vec<RuntimeEvent> {
        match &command {
            RuntimeCommand::StartTurn {
                conversation_id, ..
            } => self.handle_conversation_command(conversation_id.clone(), command, on_event),
            RuntimeCommand::InterruptTurn { .. } => {
                self.handle_interrupt_command(command, on_event)
            }
            RuntimeCommand::ApprovalResponse {
                conversation_id, ..
            } => self.handle_conversation_command(conversation_id.clone(), command, on_event),
            RuntimeCommand::Shutdown => {
                self.session_loop
                    .handle_with_event_sink(command, on_event, CancelToken::default())
            }
        }
    }

    fn handle_conversation_command(
        &self,
        conversation_id: String,
        command: RuntimeCommand,
        on_event: &mut impl FnMut(RuntimeEvent),
    ) -> Vec<RuntimeEvent> {
        let conversation_id = conversation_id.trim().to_string();
        if conversation_id.is_empty() {
            return emit_event_vec(
                RuntimeEvent::runtime_error("", "", "submission command missing conversation_id"),
                on_event,
            );
        }
        if !self
            .active_conversations
            .lock()
            .expect("active conversations poisoned")
            .insert(conversation_id.clone())
        {
            return emit_event_vec(
                RuntimeEvent::runtime_error(
                    conversation_id,
                    "",
                    "conversation already has an active runtime submission",
                ),
                on_event,
            );
        }
        let cancel_token = self.cancellations.start(&conversation_id);
        let events =
            self.session_loop
                .handle_with_event_sink(command, on_event, cancel_token.clone());
        self.cancellations.clear(&conversation_id, &cancel_token);
        self.active_conversations
            .lock()
            .expect("active conversations poisoned")
            .remove(&conversation_id);
        events
    }

    fn handle_interrupt_command(
        &self,
        command: RuntimeCommand,
        on_event: &mut impl FnMut(RuntimeEvent),
    ) -> Vec<RuntimeEvent> {
        let RuntimeCommand::InterruptTurn {
            command_id,
            conversation_id,
            reason,
            continue_after,
        } = command
        else {
            unreachable!();
        };
        let applied = self
            .cancellations
            .cancel(&conversation_id, reason.clone(), continue_after);
        if applied {
            Vec::new()
        } else {
            self.session_loop.handle_with_event_sink(
                RuntimeCommand::InterruptTurn {
                    command_id,
                    conversation_id,
                    reason,
                    continue_after,
                },
                on_event,
                CancelToken::default(),
            )
        }
    }
}

fn emit_event_vec(
    event: RuntimeEvent,
    on_event: &mut impl FnMut(RuntimeEvent),
) -> Vec<RuntimeEvent> {
    on_event(event.clone());
    vec![event]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};

    #[test]
    fn start_turn_routes_through_session_loop() {
        let loop_state = SubmissionLoop::default();
        let events = loop_state.handle(RuntimeCommand::StartTurn {
            command_id: String::new(),
            conversation_id: "conv-start-route".to_string(),
            runtime_session_id: Some("session-start-route".to_string()),
            message: "hello".to_string(),
            context: Map::new(),
        });
        assert!(events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnCompleted { .. })));
    }

    #[test]
    fn rejects_empty_conversation_id() {
        let loop_state = SubmissionLoop::default();
        let events = loop_state.handle(RuntimeCommand::StartTurn {
            command_id: String::new(),
            conversation_id: " ".to_string(),
            runtime_session_id: None,
            message: "hello".to_string(),
            context: Map::new(),
        });
        assert!(matches!(
            events.as_slice(),
            [RuntimeEvent::RuntimeError { message, .. }] if message.contains("missing conversation_id")
        ));
    }

    #[test]
    fn rejects_conversation_with_active_submission() {
        let loop_state = SubmissionLoop::default();
        loop_state
            .active_conversations
            .lock()
            .unwrap()
            .insert("conv-1".to_string());
        let events = loop_state.handle(RuntimeCommand::StartTurn {
            command_id: String::new(),
            conversation_id: "conv-1".to_string(),
            runtime_session_id: Some("session-1".to_string()),
            message: "hello".to_string(),
            context: Map::new(),
        });
        assert!(matches!(
            events.as_slice(),
            [RuntimeEvent::RuntimeError { message, .. }] if message.contains("already has an active")
        ));
    }

    #[test]
    fn interrupt_cancels_active_conversation_token() {
        let loop_state = SubmissionLoop::default();
        let token = loop_state.cancellations.start("conv-1");
        let events = loop_state.handle(RuntimeCommand::InterruptTurn {
            command_id: String::new(),
            conversation_id: "conv-1".to_string(),
            reason: "user cancelled".to_string(),
            continue_after: false,
        });

        assert!(events.is_empty());
        assert_eq!(token.abort_reason().as_deref(), Some("user cancelled"));
    }

    #[test]
    fn event_sink_receives_events_in_order() {
        let loop_state = SubmissionLoop::default();
        let mut streamed = Vec::new();
        let events = loop_state.handle_with_event_sink(
            RuntimeCommand::StartTurn {
                command_id: String::new(),
                conversation_id: "conv-event-sink".to_string(),
                runtime_session_id: Some("session-event-sink".to_string()),
                message: "hello".to_string(),
                context: Map::new(),
            },
            &mut |event| streamed.push(event),
        );

        assert_eq!(streamed, events);
        assert!(matches!(
            streamed.first(),
            Some(RuntimeEvent::SessionStarted { .. })
        ));
        assert!(matches!(
            streamed.last(),
            Some(RuntimeEvent::TurnCompleted { .. })
        ));
    }

    #[test]
    fn approval_response_routes_to_pending_session() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-submission-loop-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut context = Map::new();
        context.insert(
            "session_store_dir".to_string(),
            json!(root.to_string_lossy().to_string()),
        );
        context.insert("simulate_mcp".to_string(), json!(true));
        context.insert("approval_enabled".to_string(), json!(true));
        context.insert("mcp_enabled".to_string(), json!(true));
        context.insert(
            "mcp_tools".to_string(),
            json!([
                {"server": "demo", "name": "lookup", "enabled": true, "requires_approval": true}
            ]),
        );

        let loop_state = SubmissionLoop::default();
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

        let events = loop_state.handle(RuntimeCommand::ApprovalResponse {
            command_id: String::new(),
            conversation_id: "conv-approval".to_string(),
            runtime_session_id: Some("session-approval".to_string()),
            request_id,
            decision: "reject".to_string(),
            message: "not allowed".to_string(),
            context,
        });
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::TurnAborted { reason, .. } if reason.contains("not allowed")
        )));
        let _ = std::fs::remove_dir_all(&root);
    }
}
