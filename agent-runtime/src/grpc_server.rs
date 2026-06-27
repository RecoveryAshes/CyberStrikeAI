use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::cancellation::CancellationRegistry;
use crate::event_protocol::{RuntimeCommand, RuntimeEvent};
use crate::grpc_protocol::pb::agent_runtime_service_server::{
    AgentRuntimeService, AgentRuntimeServiceServer,
};
use crate::grpc_protocol::pb::{
    GetRunStateRequest, GetRunStateResponse, HealthRequest, HealthResponse, InterruptTurnRequest,
    InterruptTurnResponse, ListEventsRequest, ListEventsResponse, ListRunStatesRequest,
    ListRunStatesResponse, ResumeApprovalRequest, RunState, RuntimeCommand as ProtoRuntimeCommand,
    RuntimeEvent as ProtoRuntimeEvent,
};
use crate::grpc_protocol::{command_from_proto, event_to_proto};
use crate::runtime_state::{RuntimeRunState, RuntimeStateStore};
use crate::submission_loop::SubmissionLoop;

type RuntimeEventStream =
    Pin<Box<dyn futures_core::Stream<Item = Result<ProtoRuntimeEvent, Status>> + Send>>;

#[derive(Clone)]
pub struct RuntimeGrpcService {
    submission_loop: SubmissionLoop,
    cancellations: CancellationRegistry,
    runtime_state: RuntimeStateStore,
}

impl RuntimeGrpcService {
    pub fn new(
        submission_loop: SubmissionLoop,
        cancellations: CancellationRegistry,
        runtime_state: RuntimeStateStore,
    ) -> Self {
        Self {
            submission_loop,
            cancellations,
            runtime_state,
        }
    }
}

#[tonic::async_trait]
impl AgentRuntimeService for RuntimeGrpcService {
    type RunStream = RuntimeEventStream;
    type ResumeApprovalStream = RuntimeEventStream;

    async fn run(
        &self,
        request: Request<tonic::Streaming<ProtoRuntimeCommand>>,
    ) -> Result<Response<Self::RunStream>, Status> {
        let mut stream = request.into_inner();
        let command = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("missing runtime command"))?;
        let command = command_from_proto(command).map_err(internal_status)?;
        Ok(Response::new(run_command_stream(
            self.submission_loop.clone(),
            self.cancellations.clone(),
            self.runtime_state.clone(),
            command,
        )))
    }

    async fn interrupt_turn(
        &self,
        request: Request<InterruptTurnRequest>,
    ) -> Result<Response<InterruptTurnResponse>, Status> {
        let request = request.into_inner();
        let reason = if request.reason.trim().is_empty() {
            "runtime turn interrupted".to_string()
        } else {
            request.reason.clone()
        };
        self.runtime_state.request_cancel(
            &request.conversation_id,
            &reason,
            request.continue_after,
        );
        let accepted = self.cancellations.cancel(
            &request.conversation_id,
            reason.clone(),
            request.continue_after,
        );
        if accepted {
            self.runtime_state
                .mark_status(&request.conversation_id, "cancelling", reason.trim());
        }
        Ok(Response::new(InterruptTurnResponse {
            accepted,
            message: if accepted {
                "interrupt accepted".to_string()
            } else {
                "cancel signal recorded; no active turn in this runtime process".to_string()
            },
        }))
    }

    async fn resume_approval(
        &self,
        request: Request<ResumeApprovalRequest>,
    ) -> Result<Response<Self::ResumeApprovalStream>, Status> {
        let request = request.into_inner();
        if !request.raw_json.trim().is_empty() {
            let command = serde_json::from_str(&request.raw_json).map_err(internal_status)?;
            let command = fill_approval_command_from_index(command, &self.runtime_state);
            return Ok(Response::new(run_command_stream(
                self.submission_loop.clone(),
                self.cancellations.clone(),
                self.runtime_state.clone(),
                command,
            )));
        }
        let mut context = serde_json::Map::new();
        if !request.context_json.trim().is_empty() {
            context = serde_json::from_str(&request.context_json).map_err(internal_status)?;
        }
        let mut conversation_id = request.conversation_id;
        let mut runtime_session_id = request.runtime_session_id;
        let request_id = request.request_id;
        if (conversation_id.trim().is_empty() || runtime_session_id.trim().is_empty())
            && !request_id.trim().is_empty()
        {
            if let Some(index) = self.runtime_state.resolve_approval(&request_id) {
                if conversation_id.trim().is_empty() {
                    conversation_id = index.conversation_id;
                }
                if runtime_session_id.trim().is_empty() {
                    runtime_session_id = index.runtime_session_id;
                }
            }
        }
        let command = RuntimeCommand::ApprovalResponse {
            command_id: request.command_id,
            conversation_id,
            runtime_session_id: Some(runtime_session_id).filter(|s| !s.trim().is_empty()),
            request_id,
            decision: request.decision,
            message: request.message,
            context,
        };
        let command = fill_approval_command_from_index(command, &self.runtime_state);
        Ok(Response::new(run_command_stream(
            self.submission_loop.clone(),
            self.cancellations.clone(),
            self.runtime_state.clone(),
            command,
        )))
    }

    async fn get_run_state(
        &self,
        request: Request<GetRunStateRequest>,
    ) -> Result<Response<GetRunStateResponse>, Status> {
        let request = request.into_inner();
        Ok(Response::new(GetRunStateResponse {
            state: self
                .runtime_state
                .get(&request.conversation_id)
                .map(run_state_to_proto),
        }))
    }

    async fn list_run_states(
        &self,
        _request: Request<ListRunStatesRequest>,
    ) -> Result<Response<ListRunStatesResponse>, Status> {
        Ok(Response::new(ListRunStatesResponse {
            states: self
                .runtime_state
                .list_active()
                .into_iter()
                .map(run_state_to_proto)
                .collect(),
        }))
    }

    async fn list_events(
        &self,
        request: Request<ListEventsRequest>,
    ) -> Result<Response<ListEventsResponse>, Status> {
        let request = request.into_inner();
        let limit = if request.limit < 0 {
            0
        } else {
            request.limit as usize
        };
        let events = self
            .runtime_state
            .list_events(
                &request.conversation_id,
                Some(request.after_event_id.as_str()),
                limit,
            )
            .into_iter()
            .map(|stored| {
                let mut event = event_to_proto(&stored.event).map_err(internal_status)?;
                event.event_id = stored.event_id;
                Ok(event)
            })
            .collect::<Result<Vec<_>, Status>>()?;
        Ok(Response::new(ListEventsResponse { events }))
    }

    async fn health(
        &self,
        _request: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        Ok(Response::new(HealthResponse {
            ok: true,
            message: "ok".to_string(),
        }))
    }
}

pub async fn serve_grpc(
    listen: &str,
    submission_loop: SubmissionLoop,
    cancellations: CancellationRegistry,
    runtime_state: RuntimeStateStore,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind agent runtime grpc listener {listen}"))?;
    let addr = listener
        .local_addr()
        .context("read agent runtime grpc listener address")?;
    println!("agent_runtime_grpc_listen={}", public_addr(addr));
    tonic::transport::Server::builder()
        .add_service(AgentRuntimeServiceServer::new(RuntimeGrpcService::new(
            submission_loop,
            cancellations,
            runtime_state,
        )))
        .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
        .await
        .context("serve agent runtime grpc")?;
    Ok(())
}

fn fill_approval_command_from_index(
    command: RuntimeCommand,
    runtime_state: &RuntimeStateStore,
) -> RuntimeCommand {
    let RuntimeCommand::ApprovalResponse {
        command_id,
        mut conversation_id,
        runtime_session_id,
        request_id,
        decision,
        message,
        context,
    } = command
    else {
        return command;
    };
    let mut runtime_session_id = runtime_session_id.unwrap_or_default();
    if (conversation_id.trim().is_empty() || runtime_session_id.trim().is_empty())
        && !request_id.trim().is_empty()
    {
        if let Some(index) = runtime_state.resolve_approval(&request_id) {
            if conversation_id.trim().is_empty() {
                conversation_id = index.conversation_id;
            }
            if runtime_session_id.trim().is_empty() {
                runtime_session_id = index.runtime_session_id;
            }
        }
    }
    RuntimeCommand::ApprovalResponse {
        command_id,
        conversation_id,
        runtime_session_id: Some(runtime_session_id).filter(|id| !id.trim().is_empty()),
        request_id,
        decision,
        message,
        context,
    }
}

fn run_command_stream(
    submission_loop: SubmissionLoop,
    cancellations: CancellationRegistry,
    runtime_state: RuntimeStateStore,
    command: RuntimeCommand,
) -> RuntimeEventStream {
    let (tx, rx) = mpsc::channel(128);
    thread::spawn(move || {
        let completion_ids = command_completion_ids(&command);
        let cancel_conversation_id = completion_ids.1.clone();
        let guard = match runtime_state.claim_run(&cancel_conversation_id) {
            Ok(guard) => guard,
            Err(message) => {
                let _ = send_stored_event(
                    &runtime_state,
                    &tx,
                    RuntimeEvent::runtime_error(
                        cancel_conversation_id.clone(),
                        completion_ids.2.clone(),
                        message,
                    ),
                );
                let _ = send_stored_event(
                    &runtime_state,
                    &tx,
                    RuntimeEvent::CommandCompleted {
                        command_id: completion_ids.0,
                        conversation_id: completion_ids.1,
                        runtime_session_id: completion_ids.2,
                    },
                );
                return;
            }
        };
        let stop_watchdog = Arc::new(AtomicBool::new(false));
        let watchdog_stop = stop_watchdog.clone();
        let heartbeat_store = runtime_state.clone();
        let heartbeat_cancellations = cancellations.clone();
        let heartbeat_conversation_id = guard
            .as_ref()
            .map(|guard| guard.conversation_id().to_string())
            .unwrap_or_default();
        let heartbeat_owner = guard
            .as_ref()
            .map(|guard| guard.owner().to_string())
            .unwrap_or_default();
        let watchdog = thread::spawn(move || {
            while !watchdog_stop.load(Ordering::Relaxed)
                && !heartbeat_conversation_id.trim().is_empty()
            {
                thread::sleep(Duration::from_secs(2));
                heartbeat_store.heartbeat_run(&heartbeat_conversation_id, &heartbeat_owner);
                if let Some(signal) = heartbeat_store.cancel_signal(&heartbeat_conversation_id) {
                    let reason = if signal.reason.trim().is_empty() {
                        "runtime turn interrupted".to_string()
                    } else {
                        signal.reason
                    };
                    heartbeat_cancellations.cancel(
                        &heartbeat_conversation_id,
                        reason,
                        signal.continue_after,
                    );
                    heartbeat_store.mark_status(
                        &heartbeat_conversation_id,
                        "cancelling",
                        "cancel requested",
                    );
                }
            }
        });
        runtime_state.mark_command_started(&command);
        submission_loop.handle_with_event_sink(command, &mut |event| {
            runtime_state.apply_event(&event);
            let event_id = runtime_state.append_event(&event).unwrap_or_default();
            if tx
                .blocking_send(event_to_stream_item_with_id(event, event_id))
                .is_err()
            {
                if !cancel_conversation_id.trim().is_empty() {
                    runtime_state.mark_status(
                        &cancel_conversation_id,
                        "cancelling",
                        "client stream cancelled",
                    );
                    runtime_state.request_cancel(
                        &cancel_conversation_id,
                        "client stream cancelled",
                        false,
                    );
                    cancellations.cancel(
                        &cancel_conversation_id,
                        "client stream cancelled".to_string(),
                        false,
                    );
                }
            }
        });
        stop_watchdog.store(true, Ordering::Relaxed);
        let _ = watchdog.join();
        let _ = send_stored_event(
            &runtime_state,
            &tx,
            RuntimeEvent::CommandCompleted {
                command_id: completion_ids.0,
                conversation_id: completion_ids.1,
                runtime_session_id: completion_ids.2,
            },
        );
    });
    Box::pin(ReceiverStream::new(rx))
}

fn send_stored_event(
    runtime_state: &RuntimeStateStore,
    tx: &mpsc::Sender<Result<ProtoRuntimeEvent, Status>>,
    event: RuntimeEvent,
) -> Result<(), mpsc::error::SendError<Result<ProtoRuntimeEvent, Status>>> {
    runtime_state.apply_event(&event);
    let event_id = runtime_state.append_event(&event).unwrap_or_default();
    tx.blocking_send(event_to_stream_item_with_id(event, event_id))
}

fn event_to_stream_item_with_id(
    event: RuntimeEvent,
    event_id: String,
) -> Result<ProtoRuntimeEvent, Status> {
    let mut proto = event_to_proto(&event).map_err(internal_status)?;
    proto.event_id = event_id;
    proto.sequence = proto.event_id.clone();
    Ok(proto)
}

fn command_completion_ids(command: &RuntimeCommand) -> (String, String, String) {
    match command {
        RuntimeCommand::StartTurn {
            command_id,
            conversation_id,
            runtime_session_id,
            ..
        }
        | RuntimeCommand::ApprovalResponse {
            command_id,
            conversation_id,
            runtime_session_id,
            ..
        } => (
            command_id.clone(),
            conversation_id.clone(),
            runtime_session_id.clone().unwrap_or_default(),
        ),
        RuntimeCommand::InterruptTurn {
            command_id,
            conversation_id,
            ..
        } => (command_id.clone(), conversation_id.clone(), String::new()),
        RuntimeCommand::Shutdown => (String::new(), String::new(), String::new()),
    }
}

fn public_addr(addr: SocketAddr) -> String {
    if addr.ip().is_unspecified() {
        return SocketAddr::new("127.0.0.1".parse().expect("valid loopback"), addr.port())
            .to_string();
    }
    addr.to_string()
}

fn internal_status(err: impl std::fmt::Display) -> Status {
    Status::internal(err.to_string())
}

fn run_state_to_proto(state: RuntimeRunState) -> RunState {
    RunState {
        conversation_id: state.conversation_id,
        runtime_session_id: state.runtime_session_id,
        turn_id: state.turn_id,
        status: state.status,
        message: state.message,
        updated_at: state.updated_at,
        assistant_message_id: state.assistant_message_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_approval_command_uses_runtime_state_index() {
        let runtime_state = RuntimeStateStore::new(None, "test:".to_string());
        runtime_state.apply_event(&RuntimeEvent::ApprovalRequested {
            conversation_id: "conv-1".to_string(),
            runtime_session_id: "session-1".to_string(),
            turn_id: "turn-1".to_string(),
            request_id: "approval-1".to_string(),
            permission: "runtime_echo".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "runtime_echo".to_string(),
            arguments: serde_json::Value::Null,
            message: "approval needed".to_string(),
        });

        let command = fill_approval_command_from_index(
            RuntimeCommand::ApprovalResponse {
                command_id: "cmd-1".to_string(),
                conversation_id: String::new(),
                runtime_session_id: None,
                request_id: "approval-1".to_string(),
                decision: "approve".to_string(),
                message: "ok".to_string(),
                context: serde_json::Map::new(),
            },
            &runtime_state,
        );

        match command {
            RuntimeCommand::ApprovalResponse {
                conversation_id,
                runtime_session_id,
                ..
            } => {
                assert_eq!(conversation_id, "conv-1");
                assert_eq!(runtime_session_id.as_deref(), Some("session-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
