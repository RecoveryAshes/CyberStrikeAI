package handler

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"cyberstrike-ai/internal/agent"
	"cyberstrike-ai/internal/agentruntime"
	"cyberstrike-ai/internal/config"
	"cyberstrike-ai/internal/knowledge"
	"cyberstrike-ai/internal/multiagent"
	"cyberstrike-ai/internal/openai"
	"cyberstrike-ai/internal/skillpackage"

	"github.com/gin-gonic/gin"
	_ "github.com/mattn/go-sqlite3"
	"go.uber.org/zap"
)

// AgentRuntimeLoopStream streams a turn through the independent Rust Agent Runtime.
func (h *AgentHandler) AgentRuntimeLoopStream(c *gin.Context) {
	c.Header("Content-Type", "text/event-stream; charset=utf-8")
	c.Header("Cache-Control", "no-cache")
	c.Header("Connection", "keep-alive")
	c.Header("X-Accel-Buffering", "no")

	var req ChatRequest
	if err := c.ShouldBindJSON(&req); err != nil {
		writeStreamEvent(c, StreamEvent{Type: "error", Message: "请求参数错误: " + err.Error()})
		writeStreamEvent(c, StreamEvent{Type: "done", Message: ""})
		return
	}

	var writeMu sync.Mutex
	var clientDisconnected bool
	var publishConversationID string
	writeClientLine := func(line []byte) {
		if clientDisconnected {
			return
		}
		select {
		case <-c.Request.Context().Done():
			clientDisconnected = true
			return
		default:
		}
		writeMu.Lock()
		defer writeMu.Unlock()
		if _, err := c.Writer.Write(line); err != nil {
			clientDisconnected = true
			return
		}
		if flusher, ok := c.Writer.(http.Flusher); ok {
			flusher.Flush()
		} else {
			c.Writer.Flush()
		}
	}
	encodeEventLine := func(eventType, message string, data interface{}) []byte {
		ev := StreamEvent{Type: eventType, Message: message, Data: data}
		b, err := json.Marshal(ev)
		if err != nil {
			b = []byte(`{"type":"error","message":"marshal failed"}`)
		}
		line := append([]byte("data: "), b...)
		line = append(line, '\n', '\n')
		return line
	}
	sendClientEvent := func(eventType, message string, data interface{}) {
		writeClientLine(encodeEventLine(eventType, message, data))
	}
	sendEvent := func(eventType, message string, data interface{}) {
		line := encodeEventLine(eventType, message, data)
		if publishConversationID != "" && h.taskEventBus != nil {
			h.taskEventBus.Publish(publishConversationID, line)
		}
		writeClientLine(line)
	}
	publishTaskEvent := func(eventType, message string, data interface{}) {
		if h.taskEventBus == nil {
			return
		}
		cid := publishConversationID
		if m, ok := data.(map[string]interface{}); ok {
			if v, ok := m["conversationId"].(string); ok && strings.TrimSpace(v) != "" {
				cid = strings.TrimSpace(v)
			}
		}
		if cid == "" {
			return
		}
		h.taskEventBus.Publish(cid, encodeEventLine(eventType, message, data))
	}

	if h.config == nil {
		sendEvent("error", "服务器配置未加载", nil)
		sendEvent("done", "", nil)
		return
	}
	runtimeCfg := h.agentRuntimeConfig()
	if !runtimeCfg.Enabled {
		sendEvent("error", "Agent runtime 未启用，请在 agent_runtime.enabled 中开启", nil)
		sendEvent("done", "", nil)
		return
	}

	titlePublisher := sendEvent
	if req.Background {
		titlePublisher = publishTaskEvent
	}
	prep, err := h.prepareMultiAgentSessionWithTitlePublisher(&req, c, "agent_runtime_stream", titlePublisher)
	if err != nil {
		sendEvent("error", err.Error(), nil)
		sendEvent("done", "", nil)
		return
	}
	publishConversationID = prep.ConversationID
	emitStartupEvent := sendEvent
	if req.Background {
		emitStartupEvent = sendClientEvent
	}
	if prep.CreatedNew {
		emitStartupEvent("conversation", "会话已创建", map[string]interface{}{"conversationId": prep.ConversationID})
	}
	if prep.UserMessageID != "" {
		emitStartupEvent("message_saved", "", map[string]interface{}{
			"conversationId": prep.ConversationID,
			"userMessageId":  prep.UserMessageID,
		})
	}

	conversationID := prep.ConversationID
	assistantMessageID := prep.AssistantMessageID
	emitStartupEvent("progress", "正在启动独立 Agent Runtime...", map[string]interface{}{"conversationId": conversationID})

	taskStatus := "completed"
	baseCtx, cancelWithCause := context.WithCancelCause(context.Background())
	if _, err := h.tasks.StartTask(conversationID, req.Message, cancelWithCause); err != nil {
		taskStatus = "failed"
		msg := "❌ 无法启动任务: " + err.Error()
		if errors.Is(err, ErrTaskAlreadyRunning) {
			msg = "⚠️ 当前会话已有任务正在执行中，请等待当前任务完成或点击「停止任务」后再尝试。"
			emitStartupEvent("error", msg, map[string]interface{}{
				"conversationId": conversationID,
				"errorType":      "task_already_running",
			})
		} else {
			emitStartupEvent("error", msg, nil)
		}
		if assistantMessageID != "" {
			_ = h.db.UpdateAssistantMessageFinalize(assistantMessageID, msg, nil, "")
		}
		emitStartupEvent("done", "", map[string]interface{}{"conversationId": conversationID})
		return
	}
	h.tasks.SetTaskAgentMode(conversationID, "agent_runtime")
	h.tasks.SetTaskAssistantMessageID(conversationID, assistantMessageID)

	run := func(ctx context.Context, cancel context.CancelCauseFunc, emit func(eventType, message string, data interface{})) string {
		return h.executeAgentRuntimeStreamTurn(ctx, cancel, conversationID, assistantMessageID, req, prep.FinalMessage, prep.RoleTools, prep.History, emit)
	}
	if req.Background {
		taskCtx, timeoutCancel := context.WithTimeout(baseCtx, 600*time.Minute)
		go func() {
			defer timeoutCancel()
			emitRuntimeTaskEvent := func(eventType, message string, data interface{}) {
				if publishConversationID != "" && h.taskEventBus != nil {
					ev := StreamEvent{Type: eventType, Message: message, Data: data}
					b, err := json.Marshal(ev)
					if err != nil {
						b = []byte(`{"type":"error","message":"marshal failed"}`)
					}
					line := append([]byte("data: "), b...)
					line = append(line, '\n', '\n')
					h.taskEventBus.Publish(publishConversationID, line)
				}
			}
			if runtimeCfg.TransportEffective() == "grpc" {
				emitRuntimeTaskEvent = func(string, string, interface{}) {}
			}
			status := run(taskCtx, cancelWithCause, emitRuntimeTaskEvent)
			h.tasks.FinishTask(conversationID, status)
			cancelWithCause(nil)
		}()
		// Background mode returns only a startup acknowledgement on the POST response.
		// Real runtime events are published by the goroutine to TaskEventBus, where the
		// frontend's single global EventSource routes them by conversationId.
		sendClientEvent("runtime_status_update", "Agent Runtime 已在后台启动", map[string]interface{}{
			"conversationId":     conversationID,
			"runtimeEventType":   "runtime_status_update",
			"background":         true,
			"agentMode":          "agent_runtime",
			"assistantMessageId": assistantMessageID,
		})
		sendClientEvent("done", "", map[string]interface{}{"conversationId": conversationID, "background": true})
		return
	}
	taskCtx, timeoutCancel := context.WithTimeout(baseCtx, 600*time.Minute)
	defer timeoutCancel()
	defer func() {
		h.tasks.FinishTask(conversationID, taskStatus)
		cancelWithCause(nil)
	}()
	taskStatus = run(taskCtx, cancelWithCause, sendEvent)
}

func (h *AgentHandler) executeAgentRuntimeStreamTurn(
	taskCtx context.Context,
	cancelWithCause context.CancelCauseFunc,
	conversationID string,
	assistantMessageID string,
	req ChatRequest,
	finalMessage string,
	roleTools []string,
	history []agent.ChatMessage,
	sendEvent func(eventType, message string, data interface{}),
) string {
	taskStatus := "completed"
	progress := h.createProgressCallback(taskCtx, cancelWithCause, conversationID, assistantMessageID, sendEvent)
	runtimeSession, err := h.db.GetAgentRuntimeSession(conversationID)
	if err != nil {
		sendEvent("error", err.Error(), nil)
		sendEvent("done", "", map[string]interface{}{"conversationId": conversationID})
		return "failed"
	}

	runtimeSessionID := ""
	if runtimeSession != nil {
		runtimeSessionID = runtimeSession.RuntimeSessionID
	}
	client := h.agentRuntimeClient()

	var finalResponse strings.Builder
	var reasoning strings.Builder
	var lastRuntimeSessionID string
	var lastTurnID string
	var completed bool
	var aborted bool
	var pendingApproval bool

	err = client.StartTurn(taskCtx, agentruntime.Command{
		Type:             "start_turn",
		ConversationID:   conversationID,
		RuntimeSessionID: runtimeSessionID,
		Message:          finalMessage,
		Context:          agentRuntimeContextWithAssistantMessageID(h.agentRuntimeContext(conversationID, req, roleTools, history), assistantMessageID),
	}, func(event agentruntime.Event) error {
		if event.RuntimeSessionID != "" {
			lastRuntimeSessionID = event.RuntimeSessionID
		}
		if event.TurnID != "" {
			lastTurnID = event.TurnID
		}
		return h.handleAgentRuntimeEvent(progress, conversationID, assistantMessageID, event, &finalResponse, &reasoning, &completed, &aborted, &pendingApproval)
	})

	if err != nil {
		if errors.Is(context.Cause(taskCtx), ErrTaskCancelled) || errors.Is(context.Cause(taskCtx), multiagent.ErrInterruptContinue) || errors.Is(taskCtx.Err(), context.Canceled) {
			aborted = true
			taskStatus = "cancelled"
			progress("cancelled", "Agent Runtime 已取消", map[string]interface{}{"conversationId": conversationID})
			if lastRuntimeSessionID != "" {
				_ = h.db.MarkAgentRuntimeTurnFinished(conversationID, lastRuntimeSessionID, lastTurnID, "aborted")
			}
			sendEvent("done", "", map[string]interface{}{"conversationId": conversationID})
			return taskStatus
		}
		taskStatus = "failed"
		progress("error", "Agent Runtime 执行失败: "+err.Error(), map[string]interface{}{"conversationId": conversationID})
		if assistantMessageID != "" {
			_ = h.db.UpdateAssistantMessageFinalize(assistantMessageID, "Agent Runtime 执行失败: "+err.Error(), nil, strings.TrimSpace(reasoning.String()))
		}
		sendEvent("done", "", map[string]interface{}{"conversationId": conversationID})
		return taskStatus
	}

	if aborted {
		taskStatus = "cancelled"
		if lastRuntimeSessionID != "" {
			_ = h.db.MarkAgentRuntimeTurnFinished(conversationID, lastRuntimeSessionID, lastTurnID, "aborted")
		}
		sendEvent("done", "", map[string]interface{}{"conversationId": conversationID})
		return taskStatus
	}
	if pendingApproval {
		taskStatus = "completed"
		progress("progress", "Agent Runtime 已暂停，等待 HITL 审批", map[string]interface{}{"conversationId": conversationID})
		sendEvent("done", "", map[string]interface{}{"conversationId": conversationID})
		return taskStatus
	}
	if !completed {
		taskStatus = "failed"
		msg := "Agent Runtime 未返回 turn_completed"
		progress("error", msg, map[string]interface{}{"conversationId": conversationID})
		if assistantMessageID != "" {
			_ = h.db.UpdateAssistantMessageFinalize(assistantMessageID, msg, nil, strings.TrimSpace(reasoning.String()))
		}
		sendEvent("done", "", map[string]interface{}{"conversationId": conversationID})
		return taskStatus
	}

	response := strings.TrimSpace(finalResponse.String())
	if response == "" {
		response = "Agent Runtime 已完成，但未返回助手正文。"
	}
	if assistantMessageID != "" {
		if err := h.db.UpdateAssistantMessageFinalize(assistantMessageID, response, nil, strings.TrimSpace(reasoning.String())); err != nil {
			h.logger.Warn("更新 Agent Runtime 助手消息失败", zap.Error(err))
		}
	}
	if lastRuntimeSessionID != "" {
		_ = h.db.MarkAgentRuntimeTurnFinished(conversationID, lastRuntimeSessionID, lastTurnID, "completed")
	}
	sendEvent("response", response, map[string]interface{}{
		"conversationId":     conversationID,
		"assistantMessageId": assistantMessageID,
		"agentMode":          "agent_runtime",
	})
	sendEvent("done", "", map[string]interface{}{"conversationId": conversationID})
	return taskStatus
}

type agentRuntimeRunResult struct {
	Response  string
	Reasoning string
}

func (h *AgentHandler) runAgentRuntimeTurn(
	ctx context.Context,
	conversationID string,
	message string,
	req ChatRequest,
	history []agent.ChatMessage,
	roleTools []string,
	progress func(eventType, message string, data interface{}),
	assistantMessageID string,
) (*agentRuntimeRunResult, error) {
	if h == nil || h.config == nil {
		return nil, errors.New("服务器配置未加载")
	}
	if h.db == nil {
		return nil, errors.New("database is not initialized")
	}
	if !h.agentRuntimeConfig().Enabled {
		return nil, errors.New("agent runtime is disabled")
	}
	if strings.TrimSpace(conversationID) == "" {
		return nil, errors.New("conversation id is empty")
	}
	runtimeSession, err := h.db.GetAgentRuntimeSession(conversationID)
	if err != nil {
		return nil, err
	}
	runtimeSessionID := ""
	if runtimeSession != nil {
		runtimeSessionID = runtimeSession.RuntimeSessionID
	}
	if req.Message == "" {
		req.Message = message
	}
	client := h.agentRuntimeClient()
	var finalResponse strings.Builder
	var reasoning strings.Builder
	var lastRuntimeSessionID string
	var lastTurnID string
	var completed bool
	var aborted bool
	var pendingApproval bool
	err = client.StartTurn(ctx, agentruntime.Command{
		Type:             "start_turn",
		ConversationID:   conversationID,
		RuntimeSessionID: runtimeSessionID,
		Message:          message,
		Context:          agentRuntimeContextWithAssistantMessageID(h.agentRuntimeContext(conversationID, req, roleTools, history), assistantMessageID),
	}, func(event agentruntime.Event) error {
		if event.RuntimeSessionID != "" {
			lastRuntimeSessionID = event.RuntimeSessionID
		}
		if event.TurnID != "" {
			lastTurnID = event.TurnID
		}
		return h.handleAgentRuntimeEvent(progress, conversationID, assistantMessageID, event, &finalResponse, &reasoning, &completed, &aborted, &pendingApproval)
	})
	if err != nil {
		if lastRuntimeSessionID != "" {
			_ = h.db.MarkAgentRuntimeTurnFinished(conversationID, lastRuntimeSessionID, lastTurnID, "aborted")
		}
		return nil, err
	}
	if aborted {
		if lastRuntimeSessionID != "" {
			_ = h.db.MarkAgentRuntimeTurnFinished(conversationID, lastRuntimeSessionID, lastTurnID, "aborted")
		}
		return nil, errors.New("agent runtime turn aborted")
	}
	if pendingApproval {
		return nil, errors.New("agent runtime is pending HITL approval")
	}
	if !completed {
		return nil, errors.New("agent runtime did not return turn_completed")
	}
	response := strings.TrimSpace(finalResponse.String())
	if response == "" {
		response = "Agent Runtime 已完成，但未返回助手正文。"
	}
	if lastRuntimeSessionID != "" {
		_ = h.db.MarkAgentRuntimeTurnFinished(conversationID, lastRuntimeSessionID, lastTurnID, "completed")
	}
	return &agentRuntimeRunResult{
		Response:  response,
		Reasoning: strings.TrimSpace(reasoning.String()),
	}, nil
}

func (h *AgentHandler) handleAgentRuntimeEvent(
	progress func(eventType, message string, data interface{}),
	conversationID string,
	assistantMessageID string,
	event agentruntime.Event,
	finalResponse *strings.Builder,
	reasoning *strings.Builder,
	completed *bool,
	aborted *bool,
	pendingApproval *bool,
) error {
	data := h.agentRuntimeEventData(event)
	switch event.Type {
	case "session_started":
		if event.RuntimeSessionID != "" {
			_ = h.db.UpsertAgentRuntimeSession(conversationID, event.RuntimeSessionID, "", "", "started")
		}
		progress("progress", "Agent Runtime session 已启动", data)
	case "turn_started":
		if h.db != nil && h.db.DB != nil && event.RuntimeSessionID != "" && event.TurnID != "" {
			_ = h.db.MarkAgentRuntimeTurnActive(conversationID, event.RuntimeSessionID, event.TurnID)
		}
		progress("progress", "Agent Runtime turn 已启动", data)
	case "plan_updated":
		progress("planning", h.agentRuntimePlanSummary(event), data)
	case "reasoning_delta":
		reasoning.WriteString(event.Delta)
		if data != nil {
			data["streamId"] = event.TurnID
			data[openai.SSEAccumulatedKey] = reasoning.String()
		}
		progress("reasoning_chain_stream_delta", event.Delta, data)
	case "assistant_progress_update":
		if data != nil && assistantMessageID != "" {
			data["assistantMessageId"] = assistantMessageID
		}
		progress("assistant_progress_update", event.Message, data)
	case "runtime_status_update":
		progress("runtime_status_update", event.Message, data)
	case "assistant_delta":
		if event.Accumulated != "" {
			finalResponse.Reset()
			finalResponse.WriteString(event.Accumulated)
		} else {
			finalResponse.WriteString(event.Delta)
		}
		data[openai.SSEAccumulatedKey] = finalResponse.String()
		progress("response_delta", event.Delta, data)
	case "tool_call_started":
		progress("tool_call", fmt.Sprintf("调用工具: %s", event.ToolName), data)
	case "tool_call_delta":
		progress("tool_result_delta", event.Delta, data)
	case "tool_call_completed":
		progress("tool_result", event.Result, data)
	case "tool_call_failed":
		progress("tool_result", event.Error, data)
	case "approval_requested":
		*pendingApproval = true
		if h.db != nil && h.db.DB != nil && event.RuntimeSessionID != "" && event.TurnID != "" {
			_ = h.db.UpsertAgentRuntimeSession(conversationID, event.RuntimeSessionID, "", event.TurnID, "pending_approval")
		}
		if h.hitlManager != nil {
			payloadRaw, _ := json.Marshal(event.Raw)
			mode := "approval"
			if reqID := strings.TrimSpace(event.RequestID); reqID != "" {
				if p, err := h.hitlManager.CreatePendingInterruptWithID(reqID, conversationID, assistantMessageID, mode, event.ToolName, event.ToolCallID, string(payloadRaw)); err == nil {
					data["interruptId"] = p.InterruptID
					data["mode"] = mode
				} else if h.logger != nil {
					h.logger.Warn("创建 Agent Runtime HITL pending 失败", zap.Error(err), zap.String("requestID", reqID))
				}
			}
		}
		progress("hitl_approval_requested", event.Message, data)
	case "approval_resolved":
		progress("hitl_approval_resolved", event.Decision, data)
	case "follow_up_started":
		if event.Reason == "context compaction completed" {
			return nil
		}
		progress("progress", "Agent Runtime follow-up: "+event.Reason, data)
	case "compaction_started":
		progress("progress", "Agent Runtime 正在压缩上下文", data)
	case "compaction_completed":
		progress("progress", "Agent Runtime 上下文压缩完成", data)
	case "stop_hook_continued":
		progress("progress", "Agent Runtime stop hook 继续执行: "+event.Reason, data)
	case "turn_completed":
		*completed = true
		if event.Response != "" {
			finalResponse.Reset()
			finalResponse.WriteString(event.Response)
		}
		progress("progress", "Agent Runtime turn 已完成", data)
	case "turn_aborted":
		*aborted = true
		progress("cancelled", "Agent Runtime turn 已中止: "+event.Reason, data)
	case "runtime_error":
		progress("error", event.Message, data)
	default:
		progress("progress", "Agent Runtime event: "+event.Type, data)
	}
	return nil
}

func (h *AgentHandler) agentRuntimeReplayStreamEvent(event agentruntime.Event) StreamEvent {
	data := h.agentRuntimeEventData(event)
	data["replay"] = true
	data["agentMode"] = "agent_runtime"
	if event.EventID != "" {
		data["runtimeEventId"] = event.EventID
	}
	eventType := "progress"
	message := "Agent Runtime event: " + event.Type
	switch event.Type {
	case "session_started":
		message = "Agent Runtime session 已启动"
	case "turn_started":
		message = "Agent Runtime turn 已启动"
	case "plan_updated":
		eventType = "planning"
		message = h.agentRuntimePlanSummary(event)
	case "reasoning_delta":
		eventType = "reasoning_chain_stream_delta"
		message = event.Delta
		data["streamId"] = event.TurnID
		if event.Accumulated != "" {
			data[openai.SSEAccumulatedKey] = event.Accumulated
		}
	case "assistant_progress_update":
		eventType = "assistant_progress_update"
		message = event.Message
	case "runtime_status_update":
		eventType = "runtime_status_update"
		message = event.Message
	case "assistant_delta":
		eventType = "response_delta"
		message = event.Delta
		if event.Accumulated != "" {
			data[openai.SSEAccumulatedKey] = event.Accumulated
		}
	case "tool_call_started":
		eventType = "tool_call"
		message = fmt.Sprintf("调用工具: %s", event.ToolName)
	case "tool_call_delta":
		eventType = "tool_result_delta"
		message = event.Delta
	case "tool_call_completed":
		eventType = "tool_result"
		message = event.Result
	case "tool_call_failed":
		eventType = "tool_result"
		message = event.Error
	case "approval_requested":
		eventType = "hitl_approval_requested"
		message = event.Message
	case "approval_resolved":
		eventType = "hitl_approval_resolved"
		message = event.Decision
	case "follow_up_started":
		message = "Agent Runtime follow-up: " + event.Reason
	case "compaction_started":
		message = "Agent Runtime 正在压缩上下文"
	case "compaction_completed":
		message = "Agent Runtime 上下文压缩完成"
	case "stop_hook_continued":
		message = "Agent Runtime stop hook 继续执行: " + event.Reason
	case "turn_completed":
		message = "Agent Runtime turn 已完成"
	case "turn_aborted":
		eventType = "cancelled"
		message = "Agent Runtime turn 已中止: " + event.Reason
	case "runtime_error":
		eventType = "error"
		message = event.Message
	case "command_completed":
		eventType = "runtime_status_update"
		message = "Agent Runtime command 已完成"
	}
	return StreamEvent{Type: eventType, Message: message, Data: data}
}

func (h *AgentHandler) resumeAgentRuntimeApproval(ctx context.Context, requestID, decision, comment string) error {
	if h == nil || h.config == nil || h.db == nil {
		return errors.New("agent runtime handler is not initialized")
	}
	if !h.agentRuntimeConfig().Enabled {
		return errors.New("agent runtime is disabled")
	}
	interrupt, err := h.hitlManager.GetInterrupt(requestID)
	if err != nil {
		return err
	}
	if interrupt == nil {
		return fmt.Errorf("HITL interrupt not found: %s", requestID)
	}
	var runtimeSessionID string
	runtimeSession, err := h.db.GetAgentRuntimeSession(interrupt.ConversationID)
	if err != nil && h.agentRuntimeConfig().TransportEffective() != "grpc" {
		return err
	}
	if runtimeSession != nil {
		runtimeSessionID = strings.TrimSpace(runtimeSession.RuntimeSessionID)
	}
	if runtimeSessionID == "" && h.agentRuntimeConfig().TransportEffective() != "grpc" {
		return fmt.Errorf("Agent runtime session not found for conversation %s", interrupt.ConversationID)
	}

	client := h.agentRuntimeClient()
	var finalResponse strings.Builder
	var reasoning strings.Builder
	var completed bool
	var aborted bool
	var pendingApproval bool
	progress := func(eventType, message string, data interface{}) {
		if h.taskEventBus == nil || interrupt.ConversationID == "" {
			return
		}
		ev := StreamEvent{Type: eventType, Message: message, Data: data}
		b, err := json.Marshal(ev)
		if err != nil {
			return
		}
		line := append([]byte("data: "), b...)
		line = append(line, '\n', '\n')
		h.taskEventBus.Publish(interrupt.ConversationID, line)
	}
	cmdCtx, cancel := context.WithTimeout(ctx, 600*time.Minute)
	defer cancel()
	err = client.ResumeApproval(cmdCtx, agentruntime.Command{
		Type:             "approval_response",
		ConversationID:   interrupt.ConversationID,
		RuntimeSessionID: runtimeSessionID,
		RequestID:        requestID,
		Decision:         decision,
		Message:          comment,
		Context:          agentRuntimeContextWithAssistantMessageID(h.agentRuntimeContext(interrupt.ConversationID, ChatRequest{}, nil), interrupt.MessageID),
	}, func(event agentruntime.Event) error {
		return h.handleAgentRuntimeEvent(progress, interrupt.ConversationID, interrupt.MessageID, event, &finalResponse, &reasoning, &completed, &aborted, &pendingApproval)
	})
	if err != nil {
		return err
	}
	if aborted {
		_ = h.db.MarkAgentRuntimeTurnFinished(interrupt.ConversationID, runtimeSessionID, "", "aborted")
		return nil
	}
	if pendingApproval {
		return nil
	}
	if !completed {
		return errors.New("Agent Runtime approval resume did not return turn_completed")
	}
	response := strings.TrimSpace(finalResponse.String())
	if response == "" {
		response = "Agent Runtime 已完成，但未返回助手正文。"
	}
	if interrupt.MessageID != "" {
		if err := h.db.UpdateAssistantMessageFinalize(interrupt.MessageID, response, nil, strings.TrimSpace(reasoning.String())); err != nil {
			return err
		}
	}
	_ = h.db.MarkAgentRuntimeTurnFinished(interrupt.ConversationID, runtimeSessionID, "", "completed")
	progress("response", response, map[string]interface{}{
		"conversationId":     interrupt.ConversationID,
		"assistantMessageId": interrupt.MessageID,
		"agentMode":          "agent_runtime",
	})
	progress("done", "", map[string]interface{}{"conversationId": interrupt.ConversationID})
	return nil
}

func (h *AgentHandler) agentRuntimeEventData(event agentruntime.Event) map[string]interface{} {
	data := map[string]interface{}{
		"source":           "agent_runtime",
		"runtimeEventType": firstRuntimeString(event.RuntimeEventType, event.Type),
		"runtimeTrace":     agentRuntimeTraceData(event),
	}
	if event.ConversationID != "" {
		data["conversationId"] = event.ConversationID
	}
	if event.EventID != "" {
		data["runtimeEventId"] = event.EventID
	}
	if event.RuntimeSessionID != "" {
		data["runtimeSessionId"] = event.RuntimeSessionID
	}
	if event.TurnID != "" {
		data["turnId"] = event.TurnID
	}
	if event.Message != "" {
		data["message"] = event.Message
	}
	if event.Delta != "" {
		data["delta"] = event.Delta
	}
	if event.Accumulated != "" {
		data[openai.SSEAccumulatedKey] = event.Accumulated
	}
	if event.Response != "" {
		data["response"] = event.Response
		data[openai.SSEAccumulatedKey] = event.Response
	}
	if event.AssistantMessageID != "" {
		data["assistantMessageId"] = event.AssistantMessageID
	}
	if event.ToolCallID != "" {
		data["toolCallId"] = event.ToolCallID
	}
	if event.ToolName != "" {
		data["toolName"] = event.ToolName
	}
	if event.Arguments != nil {
		data["argumentsObj"] = event.Arguments
	}
	if event.Result != "" {
		data["result"] = event.Result
	}
	if event.Error != "" {
		data["error"] = event.Error
		data["success"] = false
	}
	if event.Type == "tool_call_completed" {
		data["success"] = true
	}
	if len(event.Items) > 0 {
		data["items"] = event.Items
	}
	if event.RequestID != "" {
		data["requestId"] = event.RequestID
	}
	if event.Permission != "" {
		data["permission"] = event.Permission
	}
	if event.Decision != "" {
		data["decision"] = event.Decision
	}
	if event.Summary != "" {
		data["summary"] = event.Summary
	}
	if event.ArtifactPath != "" {
		data["artifactPath"] = event.ArtifactPath
	}
	if event.ReplacementMessageCount > 0 {
		data["replacementMessageCount"] = event.ReplacementMessageCount
	}
	if event.PayloadJSON != "" {
		var payload interface{}
		if err := json.Unmarshal([]byte(event.PayloadJSON), &payload); err == nil {
			data["payload"] = payload
		}
	}
	if event.OccurredAt != "" {
		data["occurredAt"] = event.OccurredAt
	}
	if event.Sequence != "" {
		data["sequence"] = event.Sequence
	}
	return data
}

func agentRuntimeTraceData(event agentruntime.Event) map[string]interface{} {
	if strings.TrimSpace(event.RuntimeTraceJSON) != "" {
		var trace map[string]interface{}
		if err := json.Unmarshal([]byte(event.RuntimeTraceJSON), &trace); err == nil && trace != nil {
			return trace
		}
	}
	trace := map[string]interface{}{
		"schema": "cyberstrike.agent_runtime.trace.v1",
		"event":  firstRuntimeString(event.RuntimeEventType, event.Type),
	}
	if event.ConversationID != "" {
		trace["conversationId"] = event.ConversationID
	}
	if event.EventID != "" {
		trace["eventId"] = event.EventID
	}
	if event.RuntimeSessionID != "" {
		trace["runtimeSessionId"] = event.RuntimeSessionID
	}
	if event.TurnID != "" {
		trace["turnId"] = event.TurnID
	}
	if event.Message != "" {
		trace["message"] = event.Message
	}
	if event.Delta != "" {
		trace["delta"] = event.Delta
	}
	if event.ToolCallID != "" || event.ToolName != "" {
		tool := map[string]interface{}{}
		if event.ToolCallID != "" {
			tool["callId"] = event.ToolCallID
		}
		if event.ToolName != "" {
			tool["name"] = event.ToolName
			if server, name := splitExternalMCPToolName(event.ToolName); server != "" && name != "" {
				tool["kind"] = "mcp"
				tool["server"] = server
				tool["mcpName"] = name
				tool["identity"] = event.ToolName
			}
		}
		if event.Arguments != nil {
			tool["arguments"] = event.Arguments
		}
		if event.Result != "" {
			tool["result"] = event.Result
		}
		if event.Error != "" {
			tool["error"] = event.Error
		}
		trace["tool"] = tool
	}
	if len(event.Items) > 0 {
		trace["plan"] = event.Items
	}
	if event.RequestID != "" || event.Permission != "" || event.Decision != "" {
		approval := map[string]interface{}{}
		if event.RequestID != "" {
			approval["requestId"] = event.RequestID
		}
		if event.Permission != "" {
			approval["permission"] = event.Permission
		}
		if event.Decision != "" {
			approval["decision"] = event.Decision
		}
		trace["approval"] = approval
	}
	if event.Summary != "" {
		trace["summary"] = event.Summary
	}
	if event.TaskID != "" || event.Strategy != "" || event.InputMessageCount > 0 || event.InputChars > 0 || event.ReplacementMessageCount > 0 || event.ArtifactPath != "" {
		compaction := map[string]interface{}{}
		if event.TaskID != "" {
			compaction["taskId"] = event.TaskID
		}
		if event.Strategy != "" {
			compaction["strategy"] = event.Strategy
		}
		if event.InputMessageCount > 0 {
			compaction["inputMessageCount"] = event.InputMessageCount
		}
		if event.InputChars > 0 {
			compaction["inputChars"] = event.InputChars
		}
		if event.ReplacementMessageCount > 0 {
			compaction["replacementMessageCount"] = event.ReplacementMessageCount
		}
		if event.ArtifactPath != "" {
			compaction["artifact"] = map[string]interface{}{
				"kind": "compaction_checkpoint",
				"path": event.ArtifactPath,
			}
		}
		if event.Summary != "" {
			compaction["summary"] = event.Summary
		}
		trace["compaction"] = compaction
	}
	if event.Reason != "" {
		trace["reason"] = event.Reason
	}
	if event.Message != "" {
		trace["message"] = event.Message
	}
	if event.Response != "" {
		trace["response"] = event.Response
		trace[openai.SSEAccumulatedKey] = event.Response
	}
	if event.Accumulated != "" {
		trace[openai.SSEAccumulatedKey] = event.Accumulated
	}
	if event.AssistantMessageID != "" {
		trace["assistantMessageId"] = event.AssistantMessageID
	}
	return trace
}

func firstRuntimeString(values ...string) string {
	for _, value := range values {
		if strings.TrimSpace(value) != "" {
			return value
		}
	}
	return ""
}

func (h *AgentHandler) agentRuntimePlanSummary(event agentruntime.Event) string {
	if len(event.Items) == 0 {
		return "Agent Runtime plan 已更新"
	}
	lines := make([]string, 0, len(event.Items))
	for _, item := range event.Items {
		step := strings.TrimSpace(item.Step)
		if step == "" {
			continue
		}
		status := strings.TrimSpace(item.Status)
		if status == "" {
			status = "pending"
		}
		lines = append(lines, fmt.Sprintf("- [%s] %s", status, step))
	}
	if len(lines) == 0 {
		return "Agent Runtime plan 已更新"
	}
	return strings.Join(lines, "\n")
}

func writeStreamEvent(c *gin.Context, ev StreamEvent) {
	b, _ := json.Marshal(ev)
	_, _ = fmt.Fprintf(c.Writer, "data: %s\n\n", b)
	if flusher, ok := c.Writer.(http.Flusher); ok {
		flusher.Flush()
	}
}

func (h *AgentHandler) agentRuntimeClient() agentruntime.RuntimeClient {
	if h == nil || h.config == nil {
		return agentruntime.NewJSONLPersistentClient("", "")
	}
	runtimeCfg := h.agentRuntimeConfig()
	binary := runtimeCfg.BinaryPathEffective(h.configDir())
	workDir := h.agentRuntimeWorkDir()
	transport := runtimeCfg.TransportEffective()
	grpcListen := runtimeCfg.GRPCListenEffective()
	redisAddr := strings.TrimSpace(runtimeCfg.RedisAddr)
	redisPrefix := runtimeCfg.RedisPrefixEffective()
	h.agentRuntimeMu.Lock()
	defer h.agentRuntimeMu.Unlock()
	if h.agentRuntimeClientCached != nil &&
		h.agentRuntimeBinary == binary &&
		h.agentRuntimeClientWorkDir == workDir &&
		h.agentRuntimeTransport == transport &&
		h.agentRuntimeGRPCListen == grpcListen &&
		h.agentRuntimeRedisAddr == redisAddr &&
		h.agentRuntimeRedisPrefix == redisPrefix {
		return h.agentRuntimeClientCached
	}
	if h.agentRuntimeClientCached != nil {
		_ = h.agentRuntimeClientCached.Close()
	}
	if transport == "grpc" {
		h.agentRuntimeClientCached = agentruntime.NewGRPCRuntimeClient(binary, workDir, grpcListen, redisAddr, redisPrefix)
	} else {
		h.agentRuntimeClientCached = agentruntime.NewJSONLPersistentClient(binary, workDir)
	}
	h.agentRuntimeBinary = binary
	h.agentRuntimeClientWorkDir = workDir
	h.agentRuntimeTransport = transport
	h.agentRuntimeGRPCListen = grpcListen
	h.agentRuntimeRedisAddr = redisAddr
	h.agentRuntimeRedisPrefix = redisPrefix
	return h.agentRuntimeClientCached
}

func (h *AgentHandler) agentRuntimeClientIfStarted() agentruntime.RuntimeClient {
	if h == nil {
		return nil
	}
	h.agentRuntimeMu.Lock()
	defer h.agentRuntimeMu.Unlock()
	if h.agentRuntimeClientCached == nil || !h.agentRuntimeClientCached.IsStarted() {
		return nil
	}
	return h.agentRuntimeClientCached
}

func (h *AgentHandler) agentRuntimeStateReader() agentruntime.StateReader {
	if h == nil || h.config == nil {
		return nil
	}
	runtimeCfg := h.agentRuntimeConfig()
	if !runtimeCfg.Enabled || runtimeCfg.TransportEffective() != "grpc" {
		return nil
	}
	redisAddr := strings.TrimSpace(runtimeCfg.RedisAddr)
	if redisAddr == "" {
		return nil
	}
	redisPrefix := runtimeCfg.RedisPrefixEffective()
	h.agentRuntimeMu.Lock()
	defer h.agentRuntimeMu.Unlock()
	if h.agentRuntimeStateReaderCached != nil &&
		h.agentRuntimeStateRedisAddr == redisAddr &&
		h.agentRuntimeStateRedisPrefix == redisPrefix {
		return h.agentRuntimeStateReaderCached
	}
	h.agentRuntimeStateReaderCached = agentruntime.NewRedisStateReader(redisAddr, redisPrefix)
	h.agentRuntimeStateRedisAddr = redisAddr
	h.agentRuntimeStateRedisPrefix = redisPrefix
	return h.agentRuntimeStateReaderCached
}

func (h *AgentHandler) configDir() string {
	if h == nil || h.configPath == "" {
		if cwd, err := os.Getwd(); err == nil {
			return strings.TrimSpace(cwd)
		}
		return ""
	}
	dir := strings.TrimSpace(filepath.Dir(h.configPath))
	if dir == "" || dir == "." {
		if cwd, err := os.Getwd(); err == nil {
			return strings.TrimSpace(cwd)
		}
		return dir
	}
	if abs, err := filepath.Abs(dir); err == nil {
		return strings.TrimSpace(abs)
	}
	return dir
}

func (h *AgentHandler) agentRuntimeConfig() config.AgentRuntimeConfig {
	if h == nil || h.config == nil {
		return config.AgentRuntimeConfig{}
	}
	return h.config.AgentRuntimeEffective()
}

func (h *AgentHandler) agentRuntimeWorkDir() string {
	if h == nil || h.config == nil {
		return ""
	}
	runtimeCfg := h.agentRuntimeConfig()
	if p := strings.TrimSpace(runtimeCfg.WorkspaceRoot); p != "" {
		if filepath.IsAbs(p) {
			return p
		}
		if dir := h.configDir(); dir != "" {
			return filepath.Join(dir, p)
		}
		return p
	}
	return h.configDir()
}

func (h *AgentHandler) agentRuntimeContext(conversationID string, req ChatRequest, roleTools []string, history ...[]agent.ChatMessage) map[string]interface{} {
	ctx := map[string]interface{}{
		"role":                   req.Role,
		"webshell_connection_id": req.WebShellConnectionID,
	}
	if len(history) > 0 {
		ctx["conversation_history"] = h.agentRuntimeHistory(history[0])
	}
	if h != nil && h.db != nil {
		ctx["project_id"] = h.conversationProjectID(conversationID)
	}
	if h == nil || h.config == nil {
		return ctx
	}
	runtimeCfg := h.agentRuntimeConfig()
	ctx["openai_provider"] = h.config.OpenAI.Provider
	ctx["openai_api_key"] = h.config.OpenAI.APIKey
	ctx["openai_base_url"] = h.config.OpenAI.BaseURL
	ctx["openai_model"] = h.config.OpenAI.Model
	if req.Reasoning != nil {
		if effort := strings.TrimSpace(req.Reasoning.Effort); effort != "" {
			ctx["openai_reasoning_effort"] = effort
		}
	}
	ctx["max_steps"] = runtimeCfg.MaxStepsEffective()
	ctx["tool_timeout_seconds"] = runtimeCfg.ToolTimeoutSecondsEffective()
	ctx["workspace_root"] = h.agentRuntimeWorkDir()
	ctx["filesystem_enabled"] = !h.config.MultiAgent.EinoSkills.Disable && h.config.MultiAgent.EinoSkills.EinoSkillFilesystemToolsEffective()
	ctx["session_store_dir"] = filepath.Join(h.agentRuntimeWorkDir(), ".cyberstrike-agent-runtime", "sessions")
	ctx["mcp_enabled"] = runtimeCfg.MCPEnabled
	ctx["mcp_endpoint_url"] = h.agentRuntimeMCPEndpointURL()
	ctx["mcp_auth_header"] = strings.TrimSpace(h.config.MCP.AuthHeader)
	ctx["mcp_auth_header_value"] = strings.TrimSpace(h.config.MCP.AuthHeaderValue)
	ctx["mcp_tools"] = h.agentRuntimeMCPTools(roleTools)
	ctx["skills_enabled"] = runtimeCfg.SkillsEnabled
	ctx["skills_dir"] = h.agentRuntimeSkillsDir()
	ctx["skills_source"] = runtimeCfg.SkillsSourceEffective()
	ctx["skills_allowlist"] = h.agentRuntimeSkillsAllowlist(roleTools)
	if runtimeCfg.SkillsSourceEffective() == "go_context" {
		ctx["skills"] = h.agentRuntimeSkills()
	}
	ctx["knowledge_enabled"] = runtimeCfg.KnowledgeEnabled
	ctx["knowledge_snippets"] = h.agentRuntimeKnowledgeSnippets(conversationID, req.Message)
	ctx["approval_enabled"] = runtimeCfg.ApprovalEnabled && req.Hitl != nil && req.Hitl.Enabled
	ctx["approval_allowlist"] = h.agentRuntimeApprovalAllowlist(req)
	ctx["compaction_enabled"] = runtimeCfg.CompactionEnabled
	ctx["compaction_threshold_chars"] = runtimeCfg.CompactionThresholdCharsEffective()
	ctx["compaction_keep_recent_messages"] = runtimeCfg.CompactionKeepRecentMessagesEffective()
	return ctx
}

func agentRuntimeContextWithAssistantMessageID(ctx map[string]interface{}, assistantMessageID string) map[string]interface{} {
	if ctx == nil {
		ctx = map[string]interface{}{}
	}
	if assistantMessageID = strings.TrimSpace(assistantMessageID); assistantMessageID != "" {
		ctx["assistant_message_id"] = assistantMessageID
		ctx["assistantMessageId"] = assistantMessageID
	}
	return ctx
}

func (h *AgentHandler) agentRuntimeHistory(history []agent.ChatMessage) []map[string]string {
	if len(history) == 0 {
		return nil
	}
	const maxHistoryMessages = 24
	start := 0
	if len(history) > maxHistoryMessages {
		start = len(history) - maxHistoryMessages
	}
	out := make([]map[string]string, 0, len(history)-start)
	for _, msg := range history[start:] {
		role := strings.TrimSpace(strings.ToLower(msg.Role))
		if role != "user" && role != "assistant" && role != "system" && role != "tool" {
			continue
		}
		content := strings.TrimSpace(msg.Content)
		if content == "" || content == "处理中..." {
			continue
		}
		item := map[string]string{
			"role":    role,
			"content": safeTruncateString(content, 4000),
		}
		if strings.TrimSpace(msg.ReasoningContent) != "" {
			item["reasoning_content"] = safeTruncateString(msg.ReasoningContent, 2000)
		}
		out = append(out, item)
	}
	return out
}

func (h *AgentHandler) agentRuntimeApprovalAllowlist(req ChatRequest) []string {
	var items []string
	items = append(items, h.hitlConfigGlobalToolWhitelist()...)
	if req.Hitl != nil {
		items = append(items, req.Hitl.SensitiveTools...)
	}
	seen := make(map[string]struct{})
	out := make([]string, 0, len(items))
	for _, item := range items {
		trimmed := strings.TrimSpace(item)
		if trimmed == "" {
			continue
		}
		key := strings.ToLower(trimmed)
		if _, ok := seen[key]; ok {
			continue
		}
		seen[key] = struct{}{}
		out = append(out, trimmed)
	}
	return out
}

func (h *AgentHandler) agentRuntimeSkillsDir() string {
	if h == nil || h.config == nil {
		return ""
	}
	return skillpackage.SkillsRootFromConfig(h.config.SkillsDir, h.configPath)
}

func (h *AgentHandler) agentRuntimeSkillsAllowlist(roleTools []string) []string {
	_ = h
	const skillPrefix = "skill:"
	const skillQualifiedPrefix = "skill::"
	seen := make(map[string]struct{})
	out := make([]string, 0)
	for _, item := range roleTools {
		trimmed := strings.TrimSpace(item)
		if trimmed == "" {
			continue
		}
		lower := strings.ToLower(trimmed)
		if strings.HasPrefix(lower, skillQualifiedPrefix) {
			trimmed = strings.TrimSpace(trimmed[len(skillQualifiedPrefix):])
			lower = strings.ToLower(trimmed)
		} else if strings.HasPrefix(lower, skillPrefix) {
			trimmed = strings.TrimSpace(trimmed[len(skillPrefix):])
			lower = strings.ToLower(trimmed)
		} else {
			continue
		}
		if trimmed == "" {
			continue
		}
		if _, ok := seen[lower]; ok {
			continue
		}
		seen[lower] = struct{}{}
		out = append(out, trimmed)
	}
	return out
}

func (h *AgentHandler) agentRuntimeSkills() map[string]interface{} {
	out := map[string]interface{}{}
	if h == nil || h.config == nil || !h.agentRuntimeConfig().SkillsEnabled {
		return out
	}
	root := h.agentRuntimeSkillsDir()
	names, err := skillpackage.ListSkillDirNames(root)
	if err != nil {
		if h.logger != nil {
			h.logger.Debug("Agent Runtime skills scan failed", zap.Error(err), zap.String("skillsRoot", root))
		}
		return out
	}
	for _, name := range names {
		view, err := skillpackage.LoadSkill(root, name, skillpackage.LoadOptions{Depth: "full"})
		if err != nil {
			if h.logger != nil {
				h.logger.Debug("Agent Runtime skill load failed", zap.Error(err), zap.String("skill", name))
			}
			continue
		}
		content := strings.TrimSpace(view.Content)
		if content == "" {
			content = strings.TrimSpace(view.Description)
		}
		if content != "" {
			out[name] = map[string]interface{}{
				"name":          view.Name,
				"description":   view.Description,
				"content":       content,
				"base_dir":      view.Path,
				"package_files": agentRuntimeSkillPackageFiles(view.PackageFiles),
			}
		}
	}
	return out
}

func agentRuntimeSkillPackageFiles(files []skillpackage.PackageFileInfo) []map[string]interface{} {
	const maxFiles = 80
	capacity := len(files)
	if capacity > maxFiles {
		capacity = maxFiles
	}
	out := make([]map[string]interface{}, 0, capacity)
	for _, file := range files {
		if len(out) >= maxFiles {
			break
		}
		out = append(out, map[string]interface{}{
			"path":   file.Path,
			"size":   file.Size,
			"is_dir": file.IsDir,
		})
	}
	return out
}

func (h *AgentHandler) agentRuntimeMCPTools(roleTools []string) []map[string]interface{} {
	if h == nil || h.config == nil || !h.agentRuntimeConfig().MCPEnabled {
		return nil
	}

	roleToolSet := make(map[string]struct{}, len(roleTools))
	for _, tool := range roleTools {
		trimmed := strings.TrimSpace(tool)
		if trimmed != "" {
			roleToolSet[trimmed] = struct{}{}
		}
	}
	roleAllows := func(toolKey string) bool {
		if len(roleToolSet) == 0 {
			return true
		}
		_, ok := roleToolSet[toolKey]
		return ok
	}

	out := make([]map[string]interface{}, 0)
	if h.mcpServer != nil {
		for _, tool := range h.mcpServer.GetAllTools() {
			name := strings.TrimSpace(tool.Name)
			if name == "" {
				continue
			}
			if !roleAllows(name) {
				continue
			}
			out = append(out, map[string]interface{}{
				"server":            "builtin",
				"name":              name,
				"call_name":         name,
				"model_name":        name,
				"transport":         "builtin",
				"description":       agentRuntimeFirstNonEmpty(tool.ShortDescription, tool.Description),
				"input_schema":      tool.InputSchema,
				"enabled":           true,
				"requires_approval": false,
			})
		}
	}

	if h.externalMCPMgr == nil {
		return out
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	tools, err := h.externalMCPMgr.GetAllTools(ctx)
	if err != nil {
		if h.logger != nil {
			h.logger.Debug("Agent Runtime external MCP tool scan failed", zap.Error(err))
		}
		return out
	}
	cfgs := h.externalMCPMgr.GetConfigs()
	for _, tool := range tools {
		server, name := splitExternalMCPToolName(tool.Name)
		if server == "" || name == "" {
			continue
		}
		toolKey := server + "::" + name
		if !roleAllows(toolKey) {
			continue
		}
		cfg, ok := cfgs[server]
		if !ok || !externalMCPConfigEnabled(cfg) {
			continue
		}
		if cfg.ToolEnabled != nil {
			if enabled, ok := cfg.ToolEnabled[name]; ok && !enabled {
				continue
			}
		}
		out = append(out, map[string]interface{}{
			"server":            server,
			"name":              name,
			"call_name":         server + "::" + name,
			"transport":         "external",
			"description":       agentRuntimeFirstNonEmpty(tool.ShortDescription, tool.Description),
			"input_schema":      tool.InputSchema,
			"enabled":           true,
			"requires_approval": !stringInSlice(cfg.AutoApprove, name),
		})
	}
	return out
}

func (h *AgentHandler) agentRuntimeMCPEndpointURL() string {
	if h == nil || h.config == nil || !h.agentRuntimeConfig().MCPEnabled || !h.config.MCP.Enabled || h.config.MCP.Port <= 0 {
		return ""
	}
	host := strings.TrimSpace(h.config.MCP.Host)
	if host == "" || host == "0.0.0.0" || host == "::" {
		host = "127.0.0.1"
	}
	if strings.Contains(host, ":") && !strings.HasPrefix(host, "[") {
		host = "[" + host + "]"
	}
	return fmt.Sprintf("http://%s:%d/mcp", host, h.config.MCP.Port)
}

func (h *AgentHandler) agentRuntimeKnowledgeSnippets(conversationID, query string) []map[string]interface{} {
	if h == nil || h.config == nil || !h.agentRuntimeConfig().KnowledgeEnabled {
		return nil
	}
	q := strings.TrimSpace(query)
	if q == "" {
		return nil
	}
	if h.knowledgeRetriever != nil {
		ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
		defer cancel()
		results, err := h.knowledgeRetriever.Search(ctx, &knowledge.SearchRequest{Query: q, TopK: 5})
		if err == nil && len(results) > 0 {
			return agentRuntimeKnowledgeResultsToSnippets(results)
		}
		if err != nil && h.logger != nil {
			h.logger.Debug("Agent Runtime vector knowledge retrieval failed; trying sqlite fallback", zap.Error(err), zap.String("conversationID", conversationID))
		}
	}
	return h.agentRuntimeKnowledgeSnippetsFromSQLite(conversationID, q, 5)
}

func agentRuntimeKnowledgeResultsToSnippets(results []*knowledge.RetrievalResult) []map[string]interface{} {
	out := make([]map[string]interface{}, 0, len(results))
	for _, result := range results {
		if result == nil || result.Item == nil {
			continue
		}
		content := ""
		if result.Chunk != nil && strings.TrimSpace(result.Chunk.ChunkText) != "" {
			content = result.Chunk.ChunkText
		} else {
			content = result.Item.Content
		}
		out = append(out, map[string]interface{}{
			"id":       result.Item.ID,
			"title":    result.Item.Title,
			"category": result.Item.Category,
			"content":  safeTruncateString(content, 1200),
			"score":    result.Score,
		})
	}
	return out
}

func (h *AgentHandler) agentRuntimeKnowledgeSnippetsFromSQLite(conversationID, query string, limit int) []map[string]interface{} {
	dbPath := h.agentRuntimeKnowledgeDBPath()
	if dbPath == "" {
		return nil
	}
	if _, err := os.Stat(dbPath); err != nil {
		if h.logger != nil {
			h.logger.Debug("Agent Runtime knowledge sqlite fallback skipped", zap.Error(err), zap.String("path", dbPath))
		}
		return nil
	}
	db, err := sql.Open("sqlite3", dbPath)
	if err != nil {
		if h.logger != nil {
			h.logger.Debug("Agent Runtime knowledge sqlite open failed", zap.Error(err), zap.String("path", dbPath))
		}
		return nil
	}
	defer db.Close()

	if limit <= 0 {
		limit = 5
	}
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	out, err := knowledge.SearchSQLiteFallbackSnippets(ctx, db, query, limit)
	if err != nil {
		if h.logger != nil {
			h.logger.Debug("Agent Runtime knowledge sqlite fallback failed", zap.Error(err), zap.String("conversationID", conversationID))
		}
		return nil
	}
	return out
}

func (h *AgentHandler) agentRuntimeKnowledgeDBPath() string {
	if h == nil || h.config == nil {
		return ""
	}
	p := strings.TrimSpace(h.config.Database.KnowledgeDBPath)
	if p == "" {
		p = strings.TrimSpace(h.config.Database.Path)
	}
	if p == "" {
		return ""
	}
	if filepath.IsAbs(p) {
		return p
	}
	if dir := h.configDir(); dir != "" {
		return filepath.Join(dir, p)
	}
	return p
}

func splitExternalMCPToolName(fullName string) (string, string) {
	idx := strings.Index(fullName, "::")
	if idx <= 0 || idx+2 >= len(fullName) {
		return "", ""
	}
	return fullName[:idx], fullName[idx+2:]
}

func externalMCPConfigEnabled(cfg config.ExternalMCPServerConfig) bool {
	if cfg.Disabled {
		return false
	}
	return cfg.ExternalMCPEnable
}

func stringInSlice(items []string, needle string) bool {
	for _, item := range items {
		if strings.EqualFold(strings.TrimSpace(item), strings.TrimSpace(needle)) {
			return true
		}
	}
	return false
}

func agentRuntimeFirstNonEmpty(values ...string) string {
	for _, value := range values {
		if strings.TrimSpace(value) != "" {
			return strings.TrimSpace(value)
		}
	}
	return ""
}
