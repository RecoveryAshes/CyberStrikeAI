package handler

import (
	"context"
	"strings"
	"time"

	"cyberstrike-ai/internal/agentruntime"

	"go.uber.org/zap"
)

const (
	agentRuntimeTaskEventPollInterval = 750 * time.Millisecond
	agentRuntimeTaskEventCallTimeout  = 1500 * time.Millisecond
	agentRuntimeTerminalGracePeriod   = 5 * time.Second
)

// agentRuntimeTaskEventBridge is the Go HTTP/SSE facade over the Rust Agent
// Runtime event source. For gRPC transport Rust writes runtime state and event
// streams to Redis; Go reads Redis directly and keeps TaskEventBus only as the
// legacy in-process mirror.
type agentRuntimeTaskEventBridge struct {
	h                   *AgentHandler
	conversationID      string
	replayLimit         int
	cursors             map[string]string
	seen                map[string]struct{}
	assistantMessageIDs map[string]string
}

func (h *AgentHandler) newAgentRuntimeTaskEventBridge(conversationID, afterEventID string, replayLimit int) *agentRuntimeTaskEventBridge {
	conversationID = strings.TrimSpace(conversationID)
	afterEventID = strings.TrimSpace(afterEventID)
	if replayLimit <= 0 {
		replayLimit = 100
	}
	bridge := &agentRuntimeTaskEventBridge{
		h:                   h,
		conversationID:      conversationID,
		replayLimit:         replayLimit,
		cursors:             make(map[string]string),
		seen:                make(map[string]struct{}),
		assistantMessageIDs: make(map[string]string),
	}
	if conversationID != "" && afterEventID != "" {
		bridge.cursors[conversationID] = afterEventID
	}
	return bridge
}

func (b *agentRuntimeTaskEventBridge) HasConversation(ctx context.Context) bool {
	if b == nil || b.conversationID == "" {
		return true
	}
	if b.h != nil && b.h.tasks != nil && b.h.tasks.GetTask(b.conversationID) != nil {
		return true
	}
	reader := b.stateReader()
	if reader == nil {
		return false
	}
	callCtx, cancel := context.WithTimeout(ctx, agentRuntimeTaskEventCallTimeout)
	defer cancel()
	if events, err := reader.ListEvents(callCtx, b.conversationID, b.cursors[b.conversationID], b.replayLimit); err == nil && len(events) > 0 {
		return true
	}
	callCtx, cancel = context.WithTimeout(ctx, agentRuntimeTaskEventCallTimeout)
	defer cancel()
	if _, ok := b.runState(callCtx, reader, b.conversationID); ok {
		return true
	}
	return false
}

func (b *agentRuntimeTaskEventBridge) Stream(ctx context.Context, legacy <-chan []byte, writeLine func([]byte) bool) {
	if b == nil || writeLine == nil {
		return
	}
	if !b.flushRuntimeEvents(ctx, writeLine) {
		return
	}
	ticker := time.NewTicker(agentRuntimeTaskEventPollInterval)
	defer ticker.Stop()
	var legacyClosedAt time.Time
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			if !b.flushRuntimeEvents(ctx, writeLine) {
				return
			}
			if b.shouldStopAfterLegacyClosed(legacyClosedAt) {
				return
			}
		case line, ok := <-legacy:
			if !ok {
				if legacyClosedAt.IsZero() {
					legacyClosedAt = time.Now()
					legacy = nil
					if !b.flushRuntimeEvents(ctx, writeLine) {
						return
					}
					if b.shouldStopAfterLegacyClosed(legacyClosedAt) {
						return
					}
				}
				continue
			}
			if !b.writeLine(writeLine, line) {
				return
			}
		}
	}
}

func (b *agentRuntimeTaskEventBridge) flushRuntimeEvents(ctx context.Context, writeLine func([]byte) bool) bool {
	reader := b.stateReader()
	if reader == nil {
		return true
	}
	stateCtx, cancel := context.WithTimeout(ctx, agentRuntimeTaskEventCallTimeout)
	conversationIDs := b.conversationIDs(stateCtx, reader)
	cancel()
	for _, conversationID := range conversationIDs {
		eventCtx, cancel := context.WithTimeout(ctx, agentRuntimeTaskEventCallTimeout)
		events, err := reader.ListEvents(eventCtx, conversationID, b.cursors[conversationID], b.replayLimit)
		cancel()
		if err != nil {
			if b.h != nil && b.h.logger != nil {
				b.h.logger.Debug("读取 Agent Runtime Redis 事件流失败", zap.Error(err), zap.String("conversationId", conversationID))
			}
			continue
		}
		for _, event := range events {
			if strings.TrimSpace(event.ConversationID) == "" {
				event.ConversationID = conversationID
			}
			lines := b.h.agentRuntimeReplayEventLines(event)
			if len(lines) == 0 {
				continue
			}
			for _, line := range lines {
				if assistantMessageID := b.assistantMessageIDs[conversationID]; assistantMessageID != "" {
					line = ensureTaskEventDataString(line, "assistantMessageId", assistantMessageID)
				}
				if !b.writeLine(writeLine, line) {
					return false
				}
			}
		}
	}
	return true
}

func (b *agentRuntimeTaskEventBridge) writeLine(writeLine func([]byte) bool, line []byte) bool {
	if !b.rememberRuntimeEventLine(line) {
		return true
	}
	return writeLine(line)
}

func (b *agentRuntimeTaskEventBridge) shouldStopAfterLegacyClosed(closedAt time.Time) bool {
	if closedAt.IsZero() {
		return false
	}
	if b.hasSeenTerminalEvent() {
		return true
	}
	return time.Since(closedAt) >= agentRuntimeTerminalGracePeriod
}

func (b *agentRuntimeTaskEventBridge) hasSeenTerminalEvent() bool {
	if b == nil {
		return false
	}
	for key := range b.seen {
		if strings.HasSuffix(key, "|terminal") {
			return true
		}
	}
	return false
}

func (b *agentRuntimeTaskEventBridge) rememberRuntimeEventLine(line []byte) bool {
	conversationID, eventID, streamType := taskEventRuntimeIdentity(line)
	if taskEventIsTerminal(line) {
		key := conversationID + "|terminal"
		b.seen[key] = struct{}{}
	}
	if eventID == "" {
		return true
	}
	key := conversationID + "|" + eventID + "|" + streamType
	if _, exists := b.seen[key]; exists {
		return false
	}
	b.seen[key] = struct{}{}
	if conversationID != "" {
		b.cursors[conversationID] = eventID
	}
	return true
}

func (b *agentRuntimeTaskEventBridge) stateReader() agentruntime.StateReader {
	if b == nil || b.h == nil || b.h.config == nil || !b.h.agentRuntimeConfig().Enabled || b.h.agentRuntimeConfig().TransportEffective() != "grpc" {
		return nil
	}
	return b.h.agentRuntimeStateReader()
}

func (b *agentRuntimeTaskEventBridge) hasActiveAgentRuntimeTask() bool {
	if b == nil || b.h == nil || b.h.tasks == nil {
		return false
	}
	for _, task := range b.h.tasks.GetActiveTasks() {
		if task != nil && strings.TrimSpace(task.AgentMode) == "agent_runtime" {
			return true
		}
	}
	return false
}

func (b *agentRuntimeTaskEventBridge) conversationIDs(ctx context.Context, reader agentruntime.StateReader) []string {
	if b.conversationID != "" {
		if b.h != nil && b.h.tasks != nil {
			if task := b.h.tasks.GetTask(b.conversationID); task != nil {
				b.rememberAssistantMessageID(task.ConversationID, task.AssistantMessageID)
			}
		}
		if state, ok := b.runState(ctx, reader, b.conversationID); ok {
			b.rememberAssistantMessageID(state.ConversationID, state.AssistantMessageID)
		}
		return []string{b.conversationID}
	}
	seen := make(map[string]struct{})
	var out []string
	if b.h != nil && b.h.tasks != nil {
		for _, task := range b.h.tasks.GetActiveTasks() {
			if task == nil || strings.TrimSpace(task.AgentMode) != "agent_runtime" {
				continue
			}
			addConversationID(&out, seen, task.ConversationID)
			b.rememberAssistantMessageID(task.ConversationID, task.AssistantMessageID)
		}
	}
	for _, state := range b.runStates(ctx, reader) {
		addConversationID(&out, seen, state.ConversationID)
		b.rememberAssistantMessageID(state.ConversationID, state.AssistantMessageID)
	}
	return out
}

func (b *agentRuntimeTaskEventBridge) runState(ctx context.Context, reader agentruntime.StateReader, conversationID string) (agentruntime.RunState, bool) {
	if reader == nil || strings.TrimSpace(conversationID) == "" {
		return agentruntime.RunState{}, false
	}
	state, ok, err := reader.GetRunState(ctx, conversationID)
	if err != nil {
		if b.h != nil && b.h.logger != nil {
			b.h.logger.Debug("读取 Agent Runtime Redis 会话运行态失败", zap.Error(err), zap.String("conversationId", conversationID))
		}
		return agentruntime.RunState{}, false
	}
	return state, ok
}

func (b *agentRuntimeTaskEventBridge) runStates(ctx context.Context, reader agentruntime.StateReader) []agentruntime.RunState {
	if reader == nil {
		return nil
	}
	states, err := reader.ListRunStates(ctx)
	if err != nil {
		if b.h != nil && b.h.logger != nil {
			b.h.logger.Debug("读取 Agent Runtime Redis 运行态失败", zap.Error(err))
		}
		return nil
	}
	return states
}

func (b *agentRuntimeTaskEventBridge) rememberAssistantMessageID(conversationID, assistantMessageID string) {
	conversationID = strings.TrimSpace(conversationID)
	assistantMessageID = strings.TrimSpace(assistantMessageID)
	if conversationID == "" || assistantMessageID == "" {
		return
	}
	if _, exists := b.assistantMessageIDs[conversationID]; !exists {
		b.assistantMessageIDs[conversationID] = assistantMessageID
	}
}

func addConversationID(out *[]string, seen map[string]struct{}, raw string) {
	conversationID := strings.TrimSpace(raw)
	if conversationID == "" {
		return
	}
	if _, exists := seen[conversationID]; exists {
		return
	}
	seen[conversationID] = struct{}{}
	*out = append(*out, conversationID)
}
