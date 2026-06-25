use serde_json::{Map, Value};

use crate::cancellation::CancelToken;
use crate::compaction::{CompactionArtifact, CompactionRuntime};
use crate::event_protocol::RuntimeEvent;
use crate::filesystem_runtime::FilesystemRuntime;
use crate::knowledge_runtime::KnowledgeRuntime;
use crate::mcp_bridge::McpBridge;
use crate::model_stream::{ChatMessage, ModelConfig, ModelDelta, ModelStream};
use crate::permission::{PermissionDecision, PermissionPolicy};
use crate::plan_store::PlanStore;
use crate::session_store::{SessionStore, StoredCompactionArtifactRef, StoredPendingApproval};
use crate::skill_runtime::SkillRuntime;
use crate::tool_registry::{ToolExecutionContext, ToolInvocation, ToolRegistry};
use crate::tool_runtime::ToolOutcome;

#[derive(Debug)]
pub struct TurnLoop {
    conversation_id: String,
    runtime_session_id: String,
    turn_id: String,
    plan: PlanStore,
    tool_result_waiting_for_follow_up: bool,
    max_steps: usize,
}

#[derive(Debug)]
pub struct TurnRunResult {
    pub events: Vec<RuntimeEvent>,
    pub pending_approval: Option<StoredPendingApproval>,
    pub compaction_artifacts: Vec<StoredCompactionArtifactRef>,
}

impl TurnRunResult {
    fn finished(events: Vec<RuntimeEvent>) -> Self {
        Self {
            events,
            pending_approval: None,
            compaction_artifacts: Vec::new(),
        }
    }

    fn with_compactions(
        events: Vec<RuntimeEvent>,
        compaction_artifacts: Vec<StoredCompactionArtifactRef>,
    ) -> Self {
        Self {
            events,
            pending_approval: None,
            compaction_artifacts,
        }
    }

    fn pending(
        events: Vec<RuntimeEvent>,
        pending_approval: StoredPendingApproval,
        compaction_artifacts: Vec<StoredCompactionArtifactRef>,
    ) -> Self {
        Self {
            events,
            pending_approval: Some(pending_approval),
            compaction_artifacts,
        }
    }
}

impl TurnLoop {
    pub fn new(conversation_id: String, runtime_session_id: String, turn_id: String) -> Self {
        Self {
            conversation_id,
            runtime_session_id,
            turn_id,
            plan: PlanStore::default(),
            tool_result_waiting_for_follow_up: false,
            max_steps: 100,
        }
    }

    #[cfg(test)]
    pub fn run(self, message: String, context: Map<String, Value>) -> TurnRunResult {
        self.run_with_event_sink(message, context, &mut |_| {}, CancelToken::default())
    }

    pub fn run_with_event_sink(
        mut self,
        message: String,
        context: Map<String, Value>,
        on_event: &mut impl FnMut(RuntimeEvent),
        cancel_token: CancelToken,
    ) -> TurnRunResult {
        if let Some(max_steps) = context.get("max_steps").and_then(Value::as_u64) {
            self.max_steps = max_steps.max(1) as usize;
        }

        let mut events = Vec::new();
        emit_event(&mut events, on_event, self.event_turn_started());
        emit_event(
            &mut events,
            on_event,
            self.runtime_status_update("分析用户输入并准备运行上下文。"),
        );

        let skills = SkillRuntime::from_context(&context);
        let mcp = McpBridge::from_context(&context);
        let knowledge = KnowledgeRuntime::from_context(&context);
        let filesystem = FilesystemRuntime::from_context(&context);
        let registry = ToolRegistry::from_capabilities(&mcp, &filesystem);
        let model =
            ModelStream::new(ModelConfig::from_context(&context)).with_tools(registry.schemas());
        let permission = PermissionPolicy::from_context(&context);
        let mut compaction = CompactionRuntime::from_context(&context);
        let session_store = SessionStore::from_context(&context);
        let mut messages = vec![ChatMessage::system(runtime_system_prompt(
            &skills,
            &mcp,
            &knowledge,
            &filesystem,
        ))];
        let mut compaction_artifacts = Vec::new();
        messages.extend(history_messages_from_context(&context));
        messages.push(ChatMessage::user(message.clone()));

        if context
            .get("simulate_tool")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            messages.push(ChatMessage::system("SIMULATE_RUNTIME_ECHO"));
        }
        if context
            .get("simulate_skill")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            messages.push(ChatMessage::system("SIMULATE_SKILL_TOOL"));
        }
        if context
            .get("simulate_knowledge")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            messages.push(ChatMessage::system("SIMULATE_KNOWLEDGE_SEARCH"));
        }
        if context
            .get("simulate_mcp")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            messages.push(ChatMessage::system("SIMULATE_MCP_CALL"));
        }

        let mut final_response = String::new();
        let mut final_response_streamed = false;
        for _step in 0..self.max_steps {
            if let Some(reason) = cancel_token.abort_reason() {
                emit_event(&mut events, on_event, self.turn_aborted(reason));
                return TurnRunResult::with_compactions(events, compaction_artifacts);
            }
            if compaction.should_compact(&messages) {
                let task = compaction.start_task(&self.turn_id, &messages);
                emit_event(
                    &mut events,
                    on_event,
                    self.runtime_status_update("上下文接近阈值，正在压缩历史消息。"),
                );
                emit_event(&mut events, on_event, self.compaction_started(&task));
                let result = compaction.compact_with_model(&task, &messages, &self.plan, &model);
                let artifact_path = self.persist_compaction_artifact(
                    &session_store,
                    &result.artifact,
                    &mut compaction_artifacts,
                    &mut events,
                    on_event,
                );
                let replacement_message_count = result.messages.len();
                messages = result.messages;
                emit_event(
                    &mut events,
                    on_event,
                    self.compaction_completed(
                        &task,
                        result.summary,
                        replacement_message_count,
                        artifact_path,
                    ),
                );
                emit_event(
                    &mut events,
                    on_event,
                    self.runtime_status_update("上下文压缩完成，准备继续运行。"),
                );
                emit_event(
                    &mut events,
                    on_event,
                    self.follow_up_started("context compaction completed"),
                );
                continue;
            }

            let sampled_after_tool_result = self.tool_result_waiting_for_follow_up;
            let mut streamed_reasoning = String::new();
            let mut streamed_content = String::new();
            let stream_assistant_content =
                sampled_after_tool_result && !self.plan.has_active_work();
            emit_event(
                &mut events,
                on_event,
                self.runtime_status_update(progress_sampling_message(sampled_after_tool_result)),
            );
            let turn = match model.sample_with_deltas(&messages, |delta| match delta {
                ModelDelta::Content(delta) => {
                    if stream_assistant_content {
                        streamed_content.push_str(&delta);
                        final_response_streamed = true;
                        emit_event(
                            &mut events,
                            on_event,
                            self.assistant_delta(delta, &streamed_content),
                        );
                    }
                }
                ModelDelta::Reasoning(delta) => {
                    streamed_reasoning.push_str(&delta);
                    emit_event(&mut events, on_event, self.reasoning_delta(delta));
                }
            }) {
                Ok(turn) => turn,
                Err(err) => {
                    emit_event(&mut events, on_event, self.runtime_error(err.to_string()));
                    return TurnRunResult::with_compactions(events, compaction_artifacts);
                }
            };
            if sampled_after_tool_result {
                self.tool_result_waiting_for_follow_up = false;
            }
            if let Some(reason) = cancel_token.abort_reason() {
                emit_event(&mut events, on_event, self.turn_aborted(reason));
                return TurnRunResult::with_compactions(events, compaction_artifacts);
            }

            if !turn.streamed_reasoning && !turn.reasoning.trim().is_empty() {
                emit_event(
                    &mut events,
                    on_event,
                    self.reasoning_delta(turn.reasoning.clone()),
                );
            }

            let plan_was_empty_before_tools = self.plan.items().is_empty();
            if plan_was_empty_before_tools && !turn.tool_calls.iter().any(is_plan_tool_call) {
                if !turn.content.trim().is_empty() {
                    emit_event(
                        &mut events,
                        on_event,
                        self.assistant_progress_update(turn.content.clone()),
                    );
                }
                emit_event(
                    &mut events,
                    on_event,
                    self.runtime_status_update(
                        "尚未生成 Todo，继续要求模型先把自然语言需求拆解为计划。",
                    ),
                );
                emit_event(
                    &mut events,
                    on_event,
                    self.stop_hook_continued(
                        "todo plan must be created before work or final response",
                    ),
                );
                messages.push(ChatMessage::assistant(
                    content_or_none(&turn.content),
                    Vec::new(),
                ));
                messages.push(ChatMessage::system(plan_first_gate_prompt()));
                continue;
            }

            if !turn.tool_calls.is_empty() {
                if !turn.content.trim().is_empty() {
                    emit_event(
                        &mut events,
                        on_event,
                        self.assistant_progress_update(turn.content.clone()),
                    );
                }
                emit_event(
                    &mut events,
                    on_event,
                    self.runtime_status_update(progress_tool_selection_message(&turn.tool_calls)),
                );
                let executable_tool_calls =
                    executable_tool_calls_for_step(&turn.tool_calls, plan_was_empty_before_tools);
                messages.push(ChatMessage::assistant(
                    content_or_none(&turn.content),
                    executable_tool_calls.clone(),
                ));
                for call in executable_tool_calls {
                    if let Some(reason) = cancel_token.abort_reason() {
                        emit_event(&mut events, on_event, self.turn_aborted(reason));
                        return TurnRunResult::with_compactions(events, compaction_artifacts);
                    }
                    let args =
                        serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
                    let (invocation, decision) =
                        self.evaluate_tool_permission(&call, &registry, &permission);
                    match decision {
                        PermissionDecision::Allow => {}
                        PermissionDecision::Deny => {
                            let error = format!(
                                "tool denied by permission policy: {}",
                                invocation.permission_name
                            );
                            emit_event(
                                &mut events,
                                on_event,
                                self.tool_failed(&call.id, &invocation.display_name, error.clone()),
                            );
                            emit_event(&mut events, on_event, self.turn_aborted(error));
                            return TurnRunResult::with_compactions(events, compaction_artifacts);
                        }
                        PermissionDecision::RequireApproval => {
                            let request_id = format!("approval_{}", call.id);
                            emit_event(
                                &mut events,
                                on_event,
                                self.runtime_status_update(format!(
                                    "工具 {} 需要人工审批，已暂停等待处理。",
                                    invocation.display_name
                                )),
                            );
                            emit_event(
                                &mut events,
                                on_event,
                                self.approval_requested(
                                    &request_id,
                                    &invocation.permission_name,
                                    &call.id,
                                    &invocation.display_name,
                                    args.clone(),
                                    format!(
                                        "Tool {} requires human approval before execution.",
                                        invocation.display_name
                                    ),
                                ),
                            );
                            let pending = StoredPendingApproval {
                                request_id,
                                turn_id: self.turn_id.clone(),
                                tool_call: call,
                                messages,
                                plan_items: self.plan.items(),
                                context,
                            };
                            return TurnRunResult::pending(events, pending, compaction_artifacts);
                        }
                    }
                    self.execute_tool_call(
                        &call,
                        &registry,
                        &skills,
                        &mcp,
                        &knowledge,
                        &filesystem,
                        &mut messages,
                        &mut events,
                        on_event,
                    );
                }
                emit_event(
                    &mut events,
                    on_event,
                    self.runtime_status_update(if plan_was_empty_before_tools {
                        "Todo 已生成，准备按计划继续推进。"
                    } else {
                        "工具结果已写回上下文，准备继续采样。"
                    }),
                );
                emit_event(
                    &mut events,
                    on_event,
                    self.follow_up_started(if plan_was_empty_before_tools {
                        "todo plan created; continue planned work"
                    } else {
                        "tool result requires follow-up sampling"
                    }),
                );
                continue;
            }

            final_response = turn.content;
            final_response_streamed =
                final_response_streamed || (stream_assistant_content && turn.streamed_content);
            if completion_gate_allows_turn_completed(
                &self.plan,
                self.tool_result_waiting_for_follow_up,
            ) {
                emit_event(
                    &mut events,
                    on_event,
                    self.runtime_status_update("运行过程已完成，准备输出最终回复。"),
                );
                break;
            }

            let reason =
                completion_block_reason(&self.plan, self.tool_result_waiting_for_follow_up);
            emit_event(
                &mut events,
                on_event,
                self.runtime_status_update(format!("完成条件尚未满足，继续处理：{}。", reason)),
            );
            emit_event(
                &mut events,
                on_event,
                self.stop_hook_continued(reason.clone()),
            );
            messages.push(ChatMessage::assistant(
                content_or_none(&final_response),
                Vec::new(),
            ));
            messages.push(ChatMessage::system(format!(
                "ACTIVE_PLAN_BLOCKS_COMPLETION: {}. Continue the same turn. Use update_plan/todowrite if plan state must change; do not delegate next steps to the user.",
                reason
            )));
        }

        if !completion_gate_allows_turn_completed(
            &self.plan,
            self.tool_result_waiting_for_follow_up,
        ) {
            emit_event(
                &mut events,
                on_event,
                self.runtime_error("max_steps reached before completion gate passed"),
            );
            return TurnRunResult::with_compactions(events, compaction_artifacts);
        }

        let response = if final_response.trim().is_empty() {
            "Agent Runtime 已完成，但模型未返回助手正文。".to_string()
        } else {
            final_response
        };
        if !final_response_streamed {
            self.emit_assistant_text_stream(
                &response,
                &mut final_response_streamed,
                &mut events,
                on_event,
            );
        }
        emit_event(&mut events, on_event, self.turn_completed(response));
        TurnRunResult::with_compactions(events, compaction_artifacts)
    }

    pub fn resume_after_approval_with_event_sink(
        mut self,
        pending: StoredPendingApproval,
        decision: String,
        message: String,
        _resume_context: Map<String, Value>,
        on_event: &mut impl FnMut(RuntimeEvent),
        cancel_token: CancelToken,
    ) -> TurnRunResult {
        if let Err(err) = self.plan.update(pending.plan_items.clone()) {
            return TurnRunResult::finished(vec![
                self.runtime_error(format!("restore pending approval plan: {}", err))
            ]);
        }
        let mut events = Vec::new();
        emit_event(
            &mut events,
            on_event,
            RuntimeEvent::ApprovalResolved {
                conversation_id: self.conversation_id.clone(),
                runtime_session_id: self.runtime_session_id.clone(),
                turn_id: self.turn_id.clone(),
                request_id: pending.request_id.clone(),
                decision: decision.clone(),
            },
        );
        emit_event(
            &mut events,
            on_event,
            self.runtime_status_update("收到人工审批结果，准备恢复运行。"),
        );
        if decision.trim().to_lowercase() != "approve" {
            let rejection = if message.trim().is_empty() {
                "approval rejected".to_string()
            } else {
                format!("approval rejected: {}", message.trim())
            };
            emit_event(
                &mut events,
                on_event,
                self.tool_failed(
                    &pending.tool_call.id,
                    &pending.tool_call.function.name,
                    rejection.clone(),
                ),
            );
            emit_event(&mut events, on_event, self.turn_aborted(rejection));
            return TurnRunResult::finished(events);
        }

        let context = pending.context;
        if let Some(max_steps) = context.get("max_steps").and_then(Value::as_u64) {
            self.max_steps = max_steps.max(1) as usize;
        }
        let skills = SkillRuntime::from_context(&context);
        let mcp = McpBridge::from_context(&context);
        let knowledge = KnowledgeRuntime::from_context(&context);
        let filesystem = FilesystemRuntime::from_context(&context);
        let registry = ToolRegistry::from_capabilities(&mcp, &filesystem);
        let model =
            ModelStream::new(ModelConfig::from_context(&context)).with_tools(registry.schemas());
        let permission = PermissionPolicy::from_context(&context);
        let mut compaction = CompactionRuntime::from_context(&context);
        let session_store = SessionStore::from_context(&context);
        let mut messages = pending.messages;
        let mut compaction_artifacts = Vec::new();
        self.execute_tool_call(
            &pending.tool_call,
            &registry,
            &skills,
            &mcp,
            &knowledge,
            &filesystem,
            &mut messages,
            &mut events,
            on_event,
        );
        emit_event(
            &mut events,
            on_event,
            self.runtime_status_update("审批后的工具结果已写回上下文，准备继续采样。"),
        );
        emit_event(
            &mut events,
            on_event,
            self.follow_up_started("approved tool result requires follow-up sampling"),
        );

        let mut final_response = String::new();
        let mut final_response_streamed = false;
        for _step in 0..self.max_steps {
            if let Some(reason) = cancel_token.abort_reason() {
                emit_event(&mut events, on_event, self.turn_aborted(reason));
                return TurnRunResult::with_compactions(events, compaction_artifacts);
            }
            if compaction.should_compact(&messages) {
                let task = compaction.start_task(&self.turn_id, &messages);
                emit_event(
                    &mut events,
                    on_event,
                    self.runtime_status_update("上下文接近阈值，正在压缩历史消息。"),
                );
                emit_event(&mut events, on_event, self.compaction_started(&task));
                let result = compaction.compact_with_model(&task, &messages, &self.plan, &model);
                let artifact_path = self.persist_compaction_artifact(
                    &session_store,
                    &result.artifact,
                    &mut compaction_artifacts,
                    &mut events,
                    on_event,
                );
                let replacement_message_count = result.messages.len();
                messages = result.messages;
                emit_event(
                    &mut events,
                    on_event,
                    self.compaction_completed(
                        &task,
                        result.summary,
                        replacement_message_count,
                        artifact_path,
                    ),
                );
                emit_event(
                    &mut events,
                    on_event,
                    self.runtime_status_update("上下文压缩完成，准备继续运行。"),
                );
                emit_event(
                    &mut events,
                    on_event,
                    self.follow_up_started("context compaction completed"),
                );
                continue;
            }
            let sampled_after_tool_result = self.tool_result_waiting_for_follow_up;
            let mut streamed_reasoning = String::new();
            let mut streamed_content = String::new();
            let stream_assistant_content =
                sampled_after_tool_result && !self.plan.has_active_work();
            emit_event(
                &mut events,
                on_event,
                self.runtime_status_update(progress_sampling_message(sampled_after_tool_result)),
            );
            let turn = match model.sample_with_deltas(&messages, |delta| match delta {
                ModelDelta::Content(delta) => {
                    if stream_assistant_content {
                        streamed_content.push_str(&delta);
                        final_response_streamed = true;
                        emit_event(
                            &mut events,
                            on_event,
                            self.assistant_delta(delta, &streamed_content),
                        );
                    }
                }
                ModelDelta::Reasoning(delta) => {
                    streamed_reasoning.push_str(&delta);
                    emit_event(&mut events, on_event, self.reasoning_delta(delta));
                }
            }) {
                Ok(turn) => turn,
                Err(err) => {
                    emit_event(&mut events, on_event, self.runtime_error(err.to_string()));
                    return TurnRunResult::with_compactions(events, compaction_artifacts);
                }
            };
            if sampled_after_tool_result {
                self.tool_result_waiting_for_follow_up = false;
            }
            if let Some(reason) = cancel_token.abort_reason() {
                emit_event(&mut events, on_event, self.turn_aborted(reason));
                return TurnRunResult::with_compactions(events, compaction_artifacts);
            }
            if !turn.streamed_reasoning && !turn.reasoning.trim().is_empty() {
                emit_event(
                    &mut events,
                    on_event,
                    self.reasoning_delta(turn.reasoning.clone()),
                );
            }
            let plan_was_empty_before_tools = self.plan.items().is_empty();
            if plan_was_empty_before_tools && !turn.tool_calls.iter().any(is_plan_tool_call) {
                if !turn.content.trim().is_empty() {
                    emit_event(
                        &mut events,
                        on_event,
                        self.assistant_progress_update(turn.content.clone()),
                    );
                }
                emit_event(
                    &mut events,
                    on_event,
                    self.runtime_status_update(
                        "尚未生成 Todo，继续要求模型先把自然语言需求拆解为计划。",
                    ),
                );
                emit_event(
                    &mut events,
                    on_event,
                    self.stop_hook_continued(
                        "todo plan must be created before work or final response",
                    ),
                );
                messages.push(ChatMessage::assistant(
                    content_or_none(&turn.content),
                    Vec::new(),
                ));
                messages.push(ChatMessage::system(plan_first_gate_prompt()));
                continue;
            }
            if !turn.tool_calls.is_empty() {
                if !turn.content.trim().is_empty() {
                    emit_event(
                        &mut events,
                        on_event,
                        self.assistant_progress_update(turn.content.clone()),
                    );
                }
                emit_event(
                    &mut events,
                    on_event,
                    self.runtime_status_update(progress_tool_selection_message(&turn.tool_calls)),
                );
                let executable_tool_calls =
                    executable_tool_calls_for_step(&turn.tool_calls, plan_was_empty_before_tools);
                messages.push(ChatMessage::assistant(
                    content_or_none(&turn.content),
                    executable_tool_calls.clone(),
                ));
                for call in executable_tool_calls {
                    if let Some(reason) = cancel_token.abort_reason() {
                        emit_event(&mut events, on_event, self.turn_aborted(reason));
                        return TurnRunResult::with_compactions(events, compaction_artifacts);
                    }
                    let args =
                        serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
                    let (invocation, decision) =
                        self.evaluate_tool_permission(&call, &registry, &permission);
                    match decision {
                        PermissionDecision::Allow => {}
                        PermissionDecision::Deny => {
                            let error = format!(
                                "tool denied by permission policy: {}",
                                invocation.permission_name
                            );
                            emit_event(
                                &mut events,
                                on_event,
                                self.tool_failed(&call.id, &invocation.display_name, error.clone()),
                            );
                            emit_event(&mut events, on_event, self.turn_aborted(error));
                            return TurnRunResult::with_compactions(events, compaction_artifacts);
                        }
                        PermissionDecision::RequireApproval => {
                            let request_id = format!("approval_{}", call.id);
                            emit_event(
                                &mut events,
                                on_event,
                                self.runtime_status_update(format!(
                                    "工具 {} 需要人工审批，已暂停等待处理。",
                                    invocation.display_name
                                )),
                            );
                            emit_event(
                                &mut events,
                                on_event,
                                self.approval_requested(
                                    &request_id,
                                    &invocation.permission_name,
                                    &call.id,
                                    &invocation.display_name,
                                    args.clone(),
                                    format!(
                                        "Tool {} requires human approval before execution.",
                                        invocation.display_name
                                    ),
                                ),
                            );
                            let pending = StoredPendingApproval {
                                request_id,
                                turn_id: self.turn_id.clone(),
                                tool_call: call,
                                messages,
                                plan_items: self.plan.items(),
                                context,
                            };
                            return TurnRunResult::pending(events, pending, compaction_artifacts);
                        }
                    }
                    self.execute_tool_call(
                        &call,
                        &registry,
                        &skills,
                        &mcp,
                        &knowledge,
                        &filesystem,
                        &mut messages,
                        &mut events,
                        on_event,
                    );
                }
                emit_event(
                    &mut events,
                    on_event,
                    self.runtime_status_update(if plan_was_empty_before_tools {
                        "Todo 已生成，准备按计划继续推进。"
                    } else {
                        "工具结果已写回上下文，准备继续采样。"
                    }),
                );
                emit_event(
                    &mut events,
                    on_event,
                    self.follow_up_started(if plan_was_empty_before_tools {
                        "todo plan created; continue planned work"
                    } else {
                        "tool result requires follow-up sampling"
                    }),
                );
                continue;
            }
            final_response = turn.content;
            final_response_streamed =
                final_response_streamed || (stream_assistant_content && turn.streamed_content);
            if completion_gate_allows_turn_completed(
                &self.plan,
                self.tool_result_waiting_for_follow_up,
            ) {
                emit_event(
                    &mut events,
                    on_event,
                    self.runtime_status_update("运行过程已完成，准备输出最终回复。"),
                );
                break;
            }
            let reason =
                completion_block_reason(&self.plan, self.tool_result_waiting_for_follow_up);
            emit_event(
                &mut events,
                on_event,
                self.runtime_status_update(format!("完成条件尚未满足，继续处理：{}。", reason)),
            );
            emit_event(
                &mut events,
                on_event,
                self.stop_hook_continued(reason.clone()),
            );
            messages.push(ChatMessage::assistant(
                content_or_none(&final_response),
                Vec::new(),
            ));
            messages.push(ChatMessage::system(format!(
                "ACTIVE_PLAN_BLOCKS_COMPLETION: {}. Continue the same turn. Use update_plan/todowrite if plan state must change; do not delegate next steps to the user.",
                reason
            )));
        }
        if !completion_gate_allows_turn_completed(
            &self.plan,
            self.tool_result_waiting_for_follow_up,
        ) {
            emit_event(
                &mut events,
                on_event,
                self.runtime_error("max_steps reached before completion gate passed"),
            );
            return TurnRunResult::with_compactions(events, compaction_artifacts);
        }
        let response = if final_response.trim().is_empty() {
            "Agent Runtime 已完成，但模型未返回助手正文。".to_string()
        } else {
            final_response
        };
        if !final_response_streamed {
            self.emit_assistant_text_stream(
                &response,
                &mut final_response_streamed,
                &mut events,
                on_event,
            );
        }
        emit_event(&mut events, on_event, self.turn_completed(response));
        TurnRunResult::with_compactions(events, compaction_artifacts)
    }

    fn execute_tool_call(
        &mut self,
        call: &crate::model_stream::ModelToolCall,
        registry: &ToolRegistry,
        skills: &SkillRuntime,
        mcp: &McpBridge,
        knowledge: &KnowledgeRuntime,
        filesystem: &FilesystemRuntime,
        messages: &mut Vec<ChatMessage>,
        events: &mut Vec<RuntimeEvent>,
        on_event: &mut impl FnMut(RuntimeEvent),
    ) {
        let invocation = registry.invocation(call);
        let args = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
        emit_event(
            events,
            on_event,
            self.runtime_status_update(format!("开始执行工具 {}。", invocation.display_name)),
        );
        emit_event(
            events,
            on_event,
            self.tool_started(&call.id, &invocation.display_name, args),
        );
        let conversation_id = self.conversation_id.clone();
        let runtime_session_id = self.runtime_session_id.clone();
        let turn_id = self.turn_id.clone();
        let tool_call_id = call.id.clone();
        let outcome = {
            let mut tool_ctx = ToolExecutionContext {
                plan: &mut self.plan,
                skills,
                mcp,
                knowledge,
                filesystem,
            };
            let mut emit_tool_delta = |delta: String| {
                emit_event(
                    events,
                    on_event,
                    RuntimeEvent::ToolCallDelta {
                        conversation_id: conversation_id.clone(),
                        runtime_session_id: runtime_session_id.clone(),
                        turn_id: turn_id.clone(),
                        tool_call_id: tool_call_id.clone(),
                        delta,
                    },
                );
            };
            registry.execute(call, &mut tool_ctx, Some(&mut emit_tool_delta))
        };
        match outcome {
            Ok(ToolOutcome::PlanUpdated(result)) => {
                emit_event(events, on_event, self.plan_updated());
                emit_event(
                    events,
                    on_event,
                    self.runtime_status_update("Todo/计划状态已更新。"),
                );
                emit_event(
                    events,
                    on_event,
                    self.tool_completed(&call.id, &invocation.display_name, result.clone()),
                );
                messages.push(ChatMessage::tool(call.id.clone(), result));
            }
            Ok(ToolOutcome::Text(result)) => {
                emit_event(
                    events,
                    on_event,
                    self.runtime_status_update(format!(
                        "工具 {} 执行完成，已获得结果。",
                        invocation.display_name
                    )),
                );
                emit_event(
                    events,
                    on_event,
                    self.tool_completed(&call.id, &invocation.display_name, result.clone()),
                );
                messages.push(ChatMessage::tool(call.id.clone(), result));
            }
            Ok(ToolOutcome::FailedText(result)) => {
                emit_event(
                    events,
                    on_event,
                    self.runtime_status_update(format!(
                        "工具 {} 返回失败结果，已记录错误。",
                        invocation.display_name
                    )),
                );
                emit_event(
                    events,
                    on_event,
                    self.tool_failed(&call.id, &invocation.display_name, result.clone()),
                );
                messages.push(ChatMessage::tool(call.id.clone(), result));
            }
            Err(err) => {
                let error = err.to_string();
                emit_event(
                    events,
                    on_event,
                    self.runtime_status_update(format!(
                        "工具 {} 执行失败，已记录错误。",
                        invocation.display_name
                    )),
                );
                emit_event(
                    events,
                    on_event,
                    self.tool_failed(&call.id, &invocation.display_name, error.clone()),
                );
                messages.push(ChatMessage::tool(call.id.clone(), error));
            }
        }
        self.tool_result_waiting_for_follow_up = true;
    }

    fn persist_compaction_artifact(
        &self,
        session_store: &SessionStore,
        artifact: &CompactionArtifact,
        compaction_artifacts: &mut Vec<StoredCompactionArtifactRef>,
        events: &mut Vec<RuntimeEvent>,
        on_event: &mut impl FnMut(RuntimeEvent),
    ) -> String {
        match session_store.save_compaction_artifact(
            &self.conversation_id,
            &self.runtime_session_id,
            artifact,
        ) {
            Ok(Some(artifact_ref)) => {
                let path = artifact_ref.path.clone();
                compaction_artifacts.push(artifact_ref);
                path
            }
            Ok(None) => String::new(),
            Err(err) => {
                emit_event(
                    events,
                    on_event,
                    self.runtime_error(format!("save compaction artifact: {}", err)),
                );
                String::new()
            }
        }
    }

    fn evaluate_tool_permission(
        &self,
        call: &crate::model_stream::ModelToolCall,
        registry: &ToolRegistry,
        permission: &PermissionPolicy,
    ) -> (ToolInvocation, PermissionDecision) {
        let invocation = registry.invocation(call);
        let decision = permission.evaluate_invocation(&invocation);
        (invocation, decision)
    }

    fn event_turn_started(&self) -> RuntimeEvent {
        RuntimeEvent::TurnStarted {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
        }
    }

    fn plan_updated(&self) -> RuntimeEvent {
        RuntimeEvent::PlanUpdated {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            items: self.plan.event_items(),
        }
    }

    fn reasoning_delta(&self, delta: impl Into<String>) -> RuntimeEvent {
        RuntimeEvent::ReasoningDelta {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            delta: delta.into(),
        }
    }

    fn assistant_progress_update(&self, message: impl Into<String>) -> RuntimeEvent {
        RuntimeEvent::AssistantProgressUpdate {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            message: message.into(),
        }
    }

    fn runtime_status_update(&self, message: impl Into<String>) -> RuntimeEvent {
        RuntimeEvent::RuntimeStatusUpdate {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            message: message.into(),
        }
    }

    fn emit_assistant_text_stream(
        &self,
        text: &str,
        streamed: &mut bool,
        events: &mut Vec<RuntimeEvent>,
        on_event: &mut impl FnMut(RuntimeEvent),
    ) {
        if text.is_empty() {
            return;
        }
        let chunks = Self::assistant_stream_chunks(text);
        let mut accumulated = String::new();
        for chunk in chunks {
            accumulated.push_str(chunk);
            emit_event(
                events,
                on_event,
                self.assistant_delta(chunk.to_string(), &accumulated),
            );
        }
        *streamed = true;
    }

    fn assistant_stream_chunks(text: &str) -> Vec<&str> {
        if text.is_empty() {
            return Vec::new();
        }
        let mut chunks = Vec::new();
        let mut start = 0;
        for (idx, _) in text.char_indices().skip(1) {
            if idx - start >= 48 {
                chunks.push(&text[start..idx]);
                start = idx;
            }
        }
        chunks.push(&text[start..]);
        chunks
    }

    fn assistant_delta(
        &self,
        delta: impl Into<String>,
        accumulated: impl Into<String>,
    ) -> RuntimeEvent {
        RuntimeEvent::AssistantDelta {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            delta: delta.into(),
            accumulated: accumulated.into(),
        }
    }

    fn tool_started(&self, tool_call_id: &str, tool_name: &str, arguments: Value) -> RuntimeEvent {
        RuntimeEvent::ToolCallStarted {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            arguments,
        }
    }

    fn tool_completed(&self, tool_call_id: &str, tool_name: &str, result: String) -> RuntimeEvent {
        RuntimeEvent::ToolCallCompleted {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            result,
        }
    }

    fn tool_failed(&self, tool_call_id: &str, tool_name: &str, error: String) -> RuntimeEvent {
        RuntimeEvent::ToolCallFailed {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            error,
        }
    }

    fn follow_up_started(&self, reason: impl Into<String>) -> RuntimeEvent {
        RuntimeEvent::FollowUpStarted {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            reason: reason.into(),
        }
    }

    fn compaction_started(&self, task: &crate::compaction::CompactionTask) -> RuntimeEvent {
        RuntimeEvent::CompactionStarted {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            task_id: task.id.clone(),
            strategy: task.strategy.clone(),
            input_message_count: task.input_message_count,
            input_chars: task.input_chars,
        }
    }

    fn compaction_completed(
        &self,
        task: &crate::compaction::CompactionTask,
        summary: impl Into<String>,
        replacement_message_count: usize,
        artifact_path: impl Into<String>,
    ) -> RuntimeEvent {
        RuntimeEvent::CompactionCompleted {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            task_id: task.id.clone(),
            strategy: task.strategy.clone(),
            input_message_count: task.input_message_count,
            input_chars: task.input_chars,
            replacement_message_count,
            artifact_path: artifact_path.into(),
            summary: summary.into(),
        }
    }

    fn approval_requested(
        &self,
        request_id: impl Into<String>,
        permission: impl Into<String>,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: Value,
        message: impl Into<String>,
    ) -> RuntimeEvent {
        RuntimeEvent::ApprovalRequested {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            request_id: request_id.into(),
            permission: permission.into(),
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            arguments,
            message: message.into(),
        }
    }

    fn stop_hook_continued(&self, reason: impl Into<String>) -> RuntimeEvent {
        RuntimeEvent::StopHookContinued {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            reason: reason.into(),
        }
    }

    fn turn_completed(&self, response: impl Into<String>) -> RuntimeEvent {
        RuntimeEvent::TurnCompleted {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            response: response.into(),
        }
    }

    fn turn_aborted(&self, reason: impl Into<String>) -> RuntimeEvent {
        RuntimeEvent::TurnAborted {
            conversation_id: self.conversation_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
            turn_id: self.turn_id.clone(),
            reason: reason.into(),
        }
    }

    fn runtime_error(&self, message: impl Into<String>) -> RuntimeEvent {
        RuntimeEvent::runtime_error(&self.conversation_id, &self.runtime_session_id, message)
    }
}

fn progress_sampling_message(sampled_after_tool_result: bool) -> &'static str {
    if sampled_after_tool_result {
        "根据工具结果继续分析下一步。"
    } else {
        "正在请求模型分析任务。"
    }
}

fn progress_tool_selection_message(tool_calls: &[crate::model_stream::ModelToolCall]) -> String {
    let names = tool_calls
        .iter()
        .map(|call| call.function.name.trim())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    match names.as_slice() {
        [] => "模型请求执行工具。".to_string(),
        [name] => format!("模型请求执行工具 {}。", name),
        _ => format!(
            "模型请求执行 {} 个工具：{}。",
            names.len(),
            names.join(", ")
        ),
    }
}

fn runtime_system_prompt(
    skills: &SkillRuntime,
    mcp: &McpBridge,
    knowledge: &KnowledgeRuntime,
    filesystem: &FilesystemRuntime,
) -> String {
    let skill_names = skills.selected_skills();
    let skill_text = if skill_names.is_empty() {
        "No skills are currently loaded.".to_string()
    } else {
        format!("Available skills: {}.", skill_names.join(", "))
    };
    let mcp_text = if mcp.enabled_tool_count() == 0 {
        "No external MCP tools are currently enabled.".to_string()
    } else {
        format!("Available MCP tools: {}.", mcp.tool_summaries().join("; "))
    };
    let knowledge_snippets = knowledge.context_snippets();
    let knowledge_text = if knowledge_snippets.is_empty() {
        "No knowledge snippets are currently injected.".to_string()
    } else {
        format!(
            "Injected knowledge snippets:\n{}",
            knowledge_snippets.join("\n")
        )
    };
    let filesystem_text = if filesystem.is_enabled() {
        "Filesystem tools are available: ls, read_file, write_file, edit_file, glob, grep, execute. Use relative paths under the configured workspace root. Filesystem tools must be called directly as their own function tools; do not call them through mcp_call. For shell or network commands, call execute directly with {\"command\":\"...\"}.".to_string()
    } else {
        "No local filesystem tools are currently enabled.".to_string()
    };
    format!(
        "You are CyberStrikeAI Agent Runtime. Every user request, even a simple one, must first be analyzed into a concrete Todo list using update_plan/todowrite before any non-plan tool call or final answer. The Todo list must be derived from the user's natural-language request, not a generic placeholder. Keep exactly one item in_progress while work remains, and mark items completed only after the corresponding work is actually done. Do not finish or delegate next steps while any plan item is pending or in_progress. Communicate like Codex during work: for non-trivial, long-running, multi-tool, or potentially surprising work, emit short user-visible progress updates before meaningful phases and after important discoveries, explaining what you are doing now, what you found, or what you will check next. Keep these updates concise, factual, and separate from the final answer; do not reveal hidden chain-of-thought. Consecutive tool calls without progress text are allowed when that is the clearest flow, but prefer progress updates around phase changes. If a tool result was just produced, continue with follow-up sampling before final response. {} {} {} {}",
        skill_text, filesystem_text, mcp_text, knowledge_text
    )
}

fn content_or_none(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn history_messages_from_context(context: &Map<String, Value>) -> Vec<ChatMessage> {
    context
        .get("conversation_history")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let role = item
                        .get("role")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or_default();
                    if !matches!(role, "user" | "assistant" | "system" | "tool") {
                        return None;
                    }
                    let content = item
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|content| !content.is_empty())?;
                    Some(match role {
                        "user" => ChatMessage::user(content.to_string()),
                        "assistant" => {
                            ChatMessage::assistant(Some(content.to_string()), Vec::new())
                        }
                        "system" => ChatMessage::system(content.to_string()),
                        "tool" => ChatMessage::user(format!(
                            "Historical tool result transcript: {}",
                            content
                        )),
                        _ => unreachable!(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn completion_gate_allows_turn_completed(
    plan: &PlanStore,
    tool_result_waiting_for_follow_up: bool,
) -> bool {
    !plan.has_active_work() && !tool_result_waiting_for_follow_up
}

fn completion_block_reason(plan: &PlanStore, tool_result_waiting_for_follow_up: bool) -> String {
    if tool_result_waiting_for_follow_up {
        return "tool result is waiting for follow-up sampling".to_string();
    }
    if plan.has_active_work() {
        return "active plan items must be completed before turn_completed".to_string();
    }
    "completion gate is blocked".to_string()
}

fn is_plan_tool_call(call: &crate::model_stream::ModelToolCall) -> bool {
    matches!(call.function.name.trim(), "update_plan" | "todowrite")
}

fn executable_tool_calls_for_step(
    calls: &[crate::model_stream::ModelToolCall],
    plan_was_empty_before_tools: bool,
) -> Vec<crate::model_stream::ModelToolCall> {
    if plan_was_empty_before_tools {
        calls
            .iter()
            .filter(|call| is_plan_tool_call(call))
            .cloned()
            .collect()
    } else {
        calls.to_vec()
    }
}

fn plan_first_gate_prompt() -> &'static str {
    "PLAN_FIRST_REQUIRED: Before any non-plan tool call or final answer, analyze the user's natural-language request into a concrete Todo list by calling update_plan or todowrite. The plan must be specific to the user's request, not generic. Set exactly one current item to in_progress if work remains."
}

fn emit_event(
    events: &mut Vec<RuntimeEvent>,
    on_event: &mut impl FnMut(RuntimeEvent),
    event: RuntimeEvent,
) {
    on_event(event.clone());
    events.push(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_store::{PlanItem, PlanStatus};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn plan_is_completed_before_turn_completed() {
        let events = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run("请实现一个多步骤功能，需要计划".to_string(), Map::new())
            .events;
        let final_plan = events
            .iter()
            .rev()
            .find_map(|event| match event {
                RuntimeEvent::PlanUpdated { items, .. } => Some(items),
                _ => None,
            })
            .unwrap();
        assert!(final_plan.iter().all(|item| item.status == "completed"));
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::TurnCompleted { .. })
        ));
    }

    #[test]
    fn simple_request_still_gets_todo_plan_before_completion() {
        let events = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run("hello".to_string(), Map::new())
            .events;
        let first_plan = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::PlanUpdated { .. }))
            .expect("every turn should emit a model-created plan");
        let plan_tool = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::ToolCallStarted { tool_name, .. } if tool_name == "update_plan"))
            .expect("plan must be created through update_plan");
        let completed = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::TurnCompleted { .. }))
            .unwrap();
        assert!(plan_tool <= first_plan);
        assert!(first_plan < completed);

        let final_plan = events
            .iter()
            .rev()
            .find_map(|event| match event {
                RuntimeEvent::PlanUpdated { items, .. } => Some(items),
                _ => None,
            })
            .unwrap();
        assert_eq!(final_plan.len(), 3);
        assert!(final_plan.iter().all(|item| item.status == "completed"));
        assert!(final_plan.iter().any(|item| item.step.contains("hello")));
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::TurnCompleted { .. })
        ));
    }

    #[test]
    fn direct_final_answer_is_rejected_until_model_creates_todo() {
        let endpoint = start_mock_chat_server_sequence(vec![
            (
                "text/event-stream",
                "data: {\"choices\":[{\"delta\":{\"content\":\"hello directly\"}}]}\n\ndata: [DONE]\n\n",
            ),
            (
                "text/event-stream",
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[",
                    "{\"index\":0,\"id\":\"plan_1\",\"type\":\"function\",\"function\":{\"name\":\"update_plan\",\"arguments\":\"{\\\"items\\\":[{\\\"id\\\":\\\"analyze\\\",\\\"step\\\":\\\"分析用户需求：hello\\\",\\\"status\\\":\\\"completed\\\"},{\\\"id\\\":\\\"answer\\\",\\\"step\\\":\\\"回答 hello\\\",\\\"status\\\":\\\"in_progress\\\"},{\\\"id\\\":\\\"finish\\\",\\\"step\\\":\\\"完成回复\\\",\\\"status\\\":\\\"pending\\\"}]}\"}}",
                    "]}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            ),
            (
                "text/event-stream",
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[",
                    "{\"index\":0,\"id\":\"plan_2\",\"type\":\"function\",\"function\":{\"name\":\"update_plan\",\"arguments\":\"{\\\"items\\\":[{\\\"id\\\":\\\"analyze\\\",\\\"step\\\":\\\"分析用户需求：hello\\\",\\\"status\\\":\\\"completed\\\"},{\\\"id\\\":\\\"answer\\\",\\\"step\\\":\\\"回答 hello\\\",\\\"status\\\":\\\"completed\\\"},{\\\"id\\\":\\\"finish\\\",\\\"step\\\":\\\"完成回复\\\",\\\"status\\\":\\\"completed\\\"}]}\"}}",
                    "]}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            ),
            (
                "text/event-stream",
                "data: {\"choices\":[{\"delta\":{\"content\":\"hello after todo\"}}]}\n\ndata: [DONE]\n\n",
            ),
        ]);
        let mut context = Map::new();
        context.insert(
            "openai_api_key".to_string(),
            Value::String("test-key".to_string()),
        );
        context.insert(
            "openai_model".to_string(),
            Value::String("test-model".to_string()),
        );
        context.insert("openai_base_url".to_string(), Value::String(endpoint));

        let events = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run_with_event_sink(
                "hello".to_string(),
                context,
                &mut |_| {},
                CancelToken::default(),
            )
            .events;

        let rejected = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::RuntimeStatusUpdate { message, .. }
                        if message.contains("尚未生成 Todo")
                )
            })
            .expect("direct final answer should be rejected before todo");
        let plan_started = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::ToolCallStarted { tool_name, .. } if tool_name == "update_plan"))
            .expect("model should create todo through update_plan after gate");
        let completed = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::TurnCompleted { response, .. } if response == "hello after todo"))
            .unwrap();

        assert!(rejected < plan_started);
        assert!(plan_started < completed);
        assert!(!events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::AssistantDelta { accumulated, .. } if accumulated == "hello directly"
            )
        }));
    }

    #[test]
    fn active_plan_blocks_completion_gate() {
        let mut loop_state = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string());
        loop_state
            .plan
            .update(vec![PlanItem {
                id: "1".to_string(),
                step: "still running".to_string(),
                status: PlanStatus::InProgress,
                priority: None,
            }])
            .unwrap();

        assert!(!completion_gate_allows_turn_completed(
            &loop_state.plan,
            false
        ));
    }

    #[test]
    fn tool_result_triggers_follow_up_before_completion() {
        let mut context = Map::new();
        context.insert("simulate_tool".to_string(), Value::Bool(true));
        let events = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run("echo".to_string(), context)
            .events;
        let follow_up = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::FollowUpStarted { .. }))
            .unwrap();
        let completed = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::TurnCompleted { .. }))
            .unwrap();
        assert!(follow_up < completed);
    }

    #[test]
    fn premature_delegation_is_not_completed_while_plan_active() {
        let events = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run("please implement a multi step plan".to_string(), Map::new())
            .events;
        let stop_hook = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::StopHookContinued { .. }))
            .unwrap();
        let completed = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::TurnCompleted { .. }))
            .unwrap();
        assert!(stop_hook < completed);
    }

    #[test]
    fn skill_tool_result_triggers_follow_up_before_completion() {
        let mut context = Map::new();
        context.insert("simulate_skill".to_string(), Value::Bool(true));
        context.insert(
            "skills".to_string(),
            serde_json::json!({"demo": "Demo skill body"}),
        );
        let events = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run("use skill".to_string(), context)
            .events;
        let skill_result = events
            .iter()
            .position(|event| match event {
                RuntimeEvent::ToolCallCompleted {
                    tool_name, result, ..
                } => tool_name == "skill" && result.contains("Demo skill body"),
                _ => false,
            })
            .unwrap();
        let completed = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::TurnCompleted { .. }))
            .unwrap();
        assert!(skill_result < completed);
    }

    #[test]
    fn knowledge_tool_result_triggers_follow_up_before_completion() {
        let mut context = Map::new();
        context.insert("simulate_knowledge".to_string(), Value::Bool(true));
        context.insert("knowledge_enabled".to_string(), Value::Bool(true));
        context.insert(
            "knowledge_snippets".to_string(),
            serde_json::json!([
                {"id": "k1", "title": "Runtime", "category": "agent", "content": "Agent Runtime knowledge"}
            ]),
        );
        let events = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run("runtime knowledge".to_string(), context)
            .events;
        let knowledge_result = events
            .iter()
            .position(|event| match event {
                RuntimeEvent::ToolCallCompleted {
                    tool_name, result, ..
                } => tool_name == "knowledge_search" && result.contains("Agent Runtime knowledge"),
                _ => false,
            })
            .unwrap();
        let completed = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::TurnCompleted { .. }))
            .unwrap();
        assert!(knowledge_result < completed);
    }

    #[test]
    fn mcp_tool_result_triggers_follow_up_before_completion() {
        let endpoint = start_mock_mcp_server(
            r#"{"jsonrpc":"2.0","id":"cyberstrike-agent-runtime-mcp-call","result":{"content":[{"type":"text","text":"demo lookup result"}]}}"#,
        );
        let mut context = Map::new();
        context.insert("simulate_mcp".to_string(), Value::Bool(true));
        context.insert("mcp_enabled".to_string(), Value::Bool(true));
        context.insert("mcp_endpoint_url".to_string(), Value::String(endpoint));
        context.insert(
            "mcp_tools".to_string(),
            serde_json::json!([
                {"server": "demo", "name": "lookup", "enabled": true, "requires_approval": true}
            ]),
        );
        let events = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run("lookup runtime".to_string(), context.clone())
            .events;
        let mcp_result = events
            .iter()
            .position(|event| match event {
                RuntimeEvent::ToolCallCompleted {
                    tool_name, result, ..
                } => tool_name == "demo::lookup" && result.contains("demo lookup result"),
                _ => false,
            })
            .unwrap();
        let completed = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::TurnCompleted { .. }))
            .unwrap();
        assert!(mcp_result < completed);
    }

    #[test]
    fn mcp_result_is_error_emits_tool_failed_event() {
        let endpoint = start_mock_mcp_server(
            r#"{"jsonrpc":"2.0","id":"cyberstrike-agent-runtime-mcp-call","result":{"isError":true,"content":[{"type":"text","text":"demo lookup failed"}]}}"#,
        );
        let mut context = Map::new();
        context.insert("simulate_mcp".to_string(), Value::Bool(true));
        context.insert("mcp_enabled".to_string(), Value::Bool(true));
        context.insert("mcp_endpoint_url".to_string(), Value::String(endpoint));
        context.insert(
            "mcp_tools".to_string(),
            serde_json::json!([
                {"server": "demo", "name": "lookup", "enabled": true, "requires_approval": false}
            ]),
        );

        let events = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run("lookup runtime".to_string(), context)
            .events;

        assert!(events.iter().any(|event| match event {
            RuntimeEvent::ToolCallFailed {
                tool_name, error, ..
            } =>
                tool_name == "demo::lookup"
                    && error.contains("\"status\":\"failed\"")
                    && error.contains("demo lookup failed"),
            _ => false,
        }));
        assert!(!events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallCompleted { tool_name, result, .. }
                if tool_name == "demo::lookup" && result.contains("demo lookup failed")
        )));
    }

    #[test]
    fn approval_required_tool_aborts_before_execution() {
        let mut context = Map::new();
        context.insert("simulate_mcp".to_string(), Value::Bool(true));
        context.insert("approval_enabled".to_string(), Value::Bool(true));
        context.insert("mcp_enabled".to_string(), Value::Bool(true));
        context.insert(
            "mcp_tools".to_string(),
            serde_json::json!([
                {"server": "demo", "name": "lookup", "enabled": true, "requires_approval": true}
            ]),
        );
        let events = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run("lookup runtime".to_string(), context.clone())
            .events;
        assert!(events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ApprovalRequested { permission, tool_name, .. } if permission == "demo::lookup" && tool_name == "demo::lookup")));
        assert!(!events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnCompleted { .. })));
        let result = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run("lookup runtime".to_string(), context);
        assert!(result.pending_approval.is_some());
        assert!(!events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ToolCallCompleted { tool_name, .. } if tool_name == "demo::lookup")));
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

    #[test]
    fn compaction_runs_and_turn_continues() {
        let mut context = Map::new();
        context.insert("compaction_enabled".to_string(), Value::Bool(true));
        context.insert(
            "compaction_threshold_chars".to_string(),
            serde_json::json!(10),
        );
        context.insert(
            "compaction_keep_recent_messages".to_string(),
            serde_json::json!(1),
        );
        context.insert("compaction_max_per_turn".to_string(), serde_json::json!(1));
        let events = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run(
                "This message is intentionally long enough to trigger compaction before sampling."
                    .to_string(),
                context,
            )
            .events;
        let compacted = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::CompactionCompleted { .. }))
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::CompactionStarted {
                task_id,
                strategy,
                input_message_count,
                input_chars,
                ..
            } if task_id.starts_with("compaction_")
                && strategy == "rollout_summary_with_recent_tail"
                && *input_message_count > 0
                && *input_chars > 0
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::CompactionCompleted {
                input_message_count,
                input_chars,
                replacement_message_count,
                ..
            } if *input_message_count > 0 && *input_chars > 0 && *replacement_message_count > 0
        )));
        let completed = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::TurnCompleted { .. }))
            .unwrap();
        assert!(compacted < completed);
    }

    #[test]
    fn conversation_history_is_inserted_before_current_user() {
        let mut context = Map::new();
        context.insert(
            "conversation_history".to_string(),
            serde_json::json!([
                {"role": "user", "content": "previous question"},
                {"role": "assistant", "content": "previous answer"}
            ]),
        );
        let history = history_messages_from_context(&context);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].content.as_deref(), Some("previous question"));
        assert_eq!(history[1].role, "assistant");
        assert_eq!(history[1].content.as_deref(), Some("previous answer"));
    }

    #[test]
    fn conversation_history_tool_role_is_transcript_not_orphan_tool_message() {
        let mut context = Map::new();
        context.insert(
            "conversation_history".to_string(),
            serde_json::json!([
                {"role": "tool", "content": "lookup failed"},
                {"role": "assistant", "content": "I saw the tool result"}
            ]),
        );

        let history = history_messages_from_context(&context);

        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, "user");
        assert!(history[0]
            .content
            .as_deref()
            .unwrap()
            .contains("Historical tool result transcript: lookup failed"));
        assert!(history[0].tool_call_id.is_none());
    }

    #[test]
    fn streamed_model_delta_reaches_event_sink_before_completion() {
        let endpoint = start_mock_chat_server_sequence(vec![
            (
                "text/event-stream",
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[",
                    "{\"index\":0,\"id\":\"plan_1\",\"type\":\"function\",\"function\":{\"name\":\"update_plan\",\"arguments\":\"{\\\"items\\\":[{\\\"id\\\":\\\"analyze\\\",\\\"step\\\":\\\"Analyze user request: stream please\\\",\\\"status\\\":\\\"completed\\\"},{\\\"id\\\":\\\"stream\\\",\\\"step\\\":\\\"Stream response\\\",\\\"status\\\":\\\"in_progress\\\"},{\\\"id\\\":\\\"finish\\\",\\\"step\\\":\\\"Finish response\\\",\\\"status\\\":\\\"pending\\\"}]}\"}}",
                    "]}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            ),
            (
                "text/event-stream",
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[",
                    "{\"index\":0,\"id\":\"plan_2\",\"type\":\"function\",\"function\":{\"name\":\"update_plan\",\"arguments\":\"{\\\"items\\\":[{\\\"id\\\":\\\"analyze\\\",\\\"step\\\":\\\"Analyze user request: stream please\\\",\\\"status\\\":\\\"completed\\\"},{\\\"id\\\":\\\"stream\\\",\\\"step\\\":\\\"Stream response\\\",\\\"status\\\":\\\"completed\\\"},{\\\"id\\\":\\\"finish\\\",\\\"step\\\":\\\"Finish response\\\",\\\"status\\\":\\\"completed\\\"}]}\"}}",
                    "]}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            ),
            (
                "text/event-stream",
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"content\":\" stream\"}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            ),
        ]);
        let mut context = Map::new();
        context.insert(
            "openai_api_key".to_string(),
            Value::String("test-key".to_string()),
        );
        context.insert(
            "openai_model".to_string(),
            Value::String("test-model".to_string()),
        );
        context.insert("openai_base_url".to_string(), Value::String(endpoint));

        let mut sink_events = Vec::new();
        let result = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run_with_event_sink(
                "stream please".to_string(),
                context,
                &mut |event| sink_events.push(event),
                CancelToken::default(),
            );

        let first_delta = sink_events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::AssistantDelta { accumulated, .. } if accumulated == "hello"))
            .unwrap();
        let completed = sink_events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::TurnCompleted { .. }))
            .unwrap();
        assert!(first_delta < completed);
        assert_eq!(
            sink_events
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::AssistantDelta { .. }))
                .count(),
            2
        );
        assert!(matches!(
            result.events.last(),
            Some(RuntimeEvent::TurnCompleted { response, .. }) if response == "hello stream"
        ));
    }

    #[test]
    fn turn_start_status_is_runtime_status_update_not_reasoning() {
        let mut context = Map::new();
        context.insert(
            "openai_api_key".to_string(),
            Value::String("test-key".to_string()),
        );
        context.insert(
            "openai_model".to_string(),
            Value::String("test-model".to_string()),
        );
        context.insert(
            "openai_base_url".to_string(),
            Value::String(start_mock_chat_server(
                "text/event-stream",
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n",
            )),
        );

        let result = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run_with_event_sink(
                "status please".to_string(),
                context,
                &mut |_| {},
                CancelToken::default(),
            );

        assert!(result.events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::RuntimeStatusUpdate { message, .. }
                    if message == "分析用户输入并准备运行上下文。"
            )
        }));
        assert!(!result.events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::AssistantProgressUpdate { message, .. }
                    if message == "分析用户输入并准备运行上下文。"
            )
        }));
        assert!(!result.events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::ReasoningDelta { delta, .. }
                    if delta == "分析用户输入并准备运行上下文。"
            )
        }));
    }

    #[test]
    fn tool_turn_emits_structured_tool_events_and_runtime_status_updates() {
        let mut context = Map::new();
        context.insert("simulate_tool".to_string(), Value::Bool(true));

        let events = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run("inspect processes".to_string(), context)
            .events;

        let status_messages = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::RuntimeStatusUpdate { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(
            status_messages.len() >= 7,
            "expected multiple runtime status updates, got {status_messages:?}"
        );
        assert!(status_messages.contains(&"分析用户输入并准备运行上下文。"));
        assert!(status_messages.contains(&"正在请求模型分析任务。"));
        assert!(status_messages
            .iter()
            .any(|message| *message == "开始执行工具 runtime_echo。"));
        assert!(status_messages
            .iter()
            .any(|message| *message == "工具 runtime_echo 执行完成，已获得结果。"));
        assert!(status_messages.contains(&"工具结果已写回上下文，准备继续采样。"));
        assert!(status_messages.contains(&"根据工具结果继续分析下一步。"));
        assert!(status_messages.contains(&"运行过程已完成，准备输出最终回复。"));
        assert!(!events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::AssistantProgressUpdate { message, .. }
                    if status_messages.contains(&message.as_str())
            )
        }));

        let first_status = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::RuntimeStatusUpdate { .. }))
            .unwrap();
        let tool_started = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
            .unwrap();
        let final_status = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::RuntimeStatusUpdate { message, .. }
                        if message == "运行过程已完成，准备输出最终回复。"
                )
            })
            .unwrap();
        let completed = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::TurnCompleted { .. }))
            .unwrap();
        assert!(first_status < tool_started);
        assert!(final_status < completed);
    }

    #[test]
    fn tool_preamble_is_progress_update_not_final_assistant_delta() {
        let endpoint = start_mock_chat_server_sequence(vec![
            (
                "text/event-stream",
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[",
                    "{\"index\":0,\"id\":\"plan_1\",\"type\":\"function\",\"function\":{\"name\":\"update_plan\",\"arguments\":\"{\\\"items\\\":[{\\\"id\\\":\\\"analyze\\\",\\\"step\\\":\\\"Analyze user request: inspect processes\\\",\\\"status\\\":\\\"completed\\\"},{\\\"id\\\":\\\"inspect\\\",\\\"step\\\":\\\"Inspect processes\\\",\\\"status\\\":\\\"in_progress\\\"},{\\\"id\\\":\\\"finish\\\",\\\"step\\\":\\\"Finish answer\\\",\\\"status\\\":\\\"pending\\\"}]}\"}}",
                    "]}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            ),
            (
                "text/event-stream",
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"我先查看进程列表。\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"runtime_echo\",\"arguments\":\"{\\\"message\\\":\\\"ps\\\"}\"}}]}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            ),
            (
                "text/event-stream",
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[",
                    "{\"index\":0,\"id\":\"plan_2\",\"type\":\"function\",\"function\":{\"name\":\"update_plan\",\"arguments\":\"{\\\"items\\\":[{\\\"id\\\":\\\"analyze\\\",\\\"step\\\":\\\"Analyze user request: inspect processes\\\",\\\"status\\\":\\\"completed\\\"},{\\\"id\\\":\\\"inspect\\\",\\\"step\\\":\\\"Inspect processes\\\",\\\"status\\\":\\\"completed\\\"},{\\\"id\\\":\\\"finish\\\",\\\"step\\\":\\\"Finish answer\\\",\\\"status\\\":\\\"completed\\\"}]}\"}}",
                    "]}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            ),
            (
                "text/event-stream",
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n",
            ),
        ]);
        let mut context = Map::new();
        context.insert(
            "openai_api_key".to_string(),
            Value::String("test-key".to_string()),
        );
        context.insert(
            "openai_model".to_string(),
            Value::String("test-model".to_string()),
        );
        context.insert("openai_base_url".to_string(), Value::String(endpoint));

        let events = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run_with_event_sink(
                "inspect processes".to_string(),
                context,
                &mut |_| {},
                CancelToken::default(),
            )
            .events;

        let tool_started = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::ToolCallStarted { tool_name, .. } if tool_name == "runtime_echo"))
            .expect("tool should start");
        let preamble_progress = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::AssistantProgressUpdate { message, .. }
                        if message == "我先查看进程列表。"
                )
            })
            .expect("tool preamble should be assistant progress");

        assert!(preamble_progress < tool_started);
        assert!(!events.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::AssistantDelta { delta, .. } if delta == "我先查看进程列表。"
            )
        }));
    }

    #[test]
    fn consecutive_tool_calls_do_not_require_progress_text_between_them() {
        let mut context = Map::new();
        context.insert("simulate_tool".to_string(), Value::Bool(true));

        let events = TurnLoop::new("c".to_string(), "s".to_string(), "t".to_string())
            .run_with_event_sink(
                "run two tools".to_string(),
                context,
                &mut |_| {},
                CancelToken::default(),
            )
            .events;

        let started = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ToolCallStarted { tool_name, .. } => Some(tool_name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(started, vec!["update_plan", "runtime_echo", "update_plan"]);
        assert!(!events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::AssistantProgressUpdate { .. })));
    }

    #[test]
    fn runtime_system_prompt_encourages_codex_style_progress_updates() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-runtime-prompt-fs-test-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&root);
        let mut fs_context = Map::new();
        fs_context.insert("filesystem_enabled".to_string(), Value::Bool(true));
        fs_context.insert(
            "workspace_root".to_string(),
            Value::String(root.to_string_lossy().to_string()),
        );
        let prompt = runtime_system_prompt(
            &SkillRuntime::default(),
            &McpBridge::from_context(&Map::new()),
            &KnowledgeRuntime::from_context(&Map::new()),
            &FilesystemRuntime::from_context(&fs_context),
        );

        assert!(prompt.contains("Every user request, even a simple one"));
        assert!(prompt
            .contains("using update_plan/todowrite before any non-plan tool call or final answer"));
        assert!(prompt.contains("not a generic placeholder"));
        assert!(prompt.contains("Keep exactly one item in_progress"));
        assert!(prompt.contains("Communicate like Codex during work"));
        assert!(prompt.contains("emit short user-visible progress updates"));
        assert!(
            prompt.contains("what you are doing now, what you found, or what you will check next")
        );
        assert!(prompt.contains("Consecutive tool calls without progress text are allowed"));
        assert!(prompt.contains("prefer progress updates around phase changes"));
        assert!(prompt.contains("do not reveal hidden chain-of-thought"));
        assert!(prompt.contains("Filesystem tools must be called directly"));
        assert!(prompt.contains("do not call them through mcp_call"));
        assert!(prompt.contains("call execute directly with {\"command\":\"...\"}"));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn start_mock_chat_server(content_type: &'static str, response_body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 8192];
            let bytes = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]);
            assert!(request.contains("POST /chat/completions HTTP/1.1"));
            assert!(request.contains("\"stream\":true"));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                content_type,
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        format!("http://{}", addr)
    }

    fn start_mock_chat_server_sequence(responses: Vec<(&'static str, &'static str)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for (content_type, response_body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0_u8; 8192];
                let bytes = stream.read(&mut buffer).unwrap();
                let request = String::from_utf8_lossy(&buffer[..bytes]);
                assert!(request.contains("POST /chat/completions HTTP/1.1"));
                assert!(request.contains("\"stream\":true"));
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    content_type,
                    response_body.len(),
                    response_body
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        format!("http://{}", addr)
    }
}
