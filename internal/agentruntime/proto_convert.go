package agentruntime

import (
	"encoding/json"
	"fmt"

	agentruntimepb "cyberstrike-ai/internal/agentruntime/pb"
)

func commandToProto(cmd Command) (*agentruntimepb.RuntimeCommand, error) {
	prepareCommand(&cmd)
	contextJSON, err := marshalJSONMap(cmd.Context)
	if err != nil {
		return nil, fmt.Errorf("marshal runtime command context: %w", err)
	}
	rawJSON, err := json.Marshal(cmd)
	if err != nil {
		return nil, fmt.Errorf("marshal runtime command raw json: %w", err)
	}
	return &agentruntimepb.RuntimeCommand{
		Type:             cmd.Type,
		CommandId:        cmd.CommandID,
		ConversationId:   cmd.ConversationID,
		RuntimeSessionId: cmd.RuntimeSessionID,
		Message:          cmd.Message,
		ContextJson:      contextJSON,
		Reason:           cmd.Reason,
		ContinueAfter:    cmd.ContinueAfter,
		RequestId:        cmd.RequestID,
		Decision:         cmd.Decision,
		ApprovalMessage:  cmd.Message,
		RawJson:          string(rawJSON),
	}, nil
}

func protoToCommand(pb *agentruntimepb.RuntimeCommand) (Command, error) {
	if pb == nil {
		return Command{}, fmt.Errorf("runtime command is nil")
	}
	var ctx map[string]interface{}
	if pb.ContextJson != "" {
		if err := json.Unmarshal([]byte(pb.ContextJson), &ctx); err != nil {
			return Command{}, fmt.Errorf("decode runtime command context: %w", err)
		}
	}
	cmd := Command{
		Type:             pb.Type,
		CommandID:        pb.CommandId,
		ConversationID:   pb.ConversationId,
		RuntimeSessionID: pb.RuntimeSessionId,
		Message:          pb.Message,
		Context:          ctx,
		Reason:           pb.Reason,
		ContinueAfter:    pb.ContinueAfter,
		RequestID:        pb.RequestId,
		Decision:         pb.Decision,
	}
	if cmd.Type == "approval_response" && pb.ApprovalMessage != "" {
		cmd.Message = pb.ApprovalMessage
	}
	if pb.RawJson != "" {
		_ = json.Unmarshal([]byte(pb.RawJson), &cmd)
		if cmd.Context == nil {
			cmd.Context = ctx
		}
	}
	prepareCommand(&cmd)
	return cmd, nil
}

func eventToProto(event Event) (*agentruntimepb.RuntimeEvent, error) {
	argumentsJSON, err := marshalJSONMap(event.Arguments)
	if err != nil {
		return nil, fmt.Errorf("marshal runtime event arguments: %w", err)
	}
	raw := event.Raw
	if raw == nil {
		rawBytes, err := json.Marshal(event)
		if err != nil {
			return nil, fmt.Errorf("marshal runtime event raw json: %w", err)
		}
		_ = json.Unmarshal(rawBytes, &raw)
	}
	rawJSON, err := json.Marshal(raw)
	if err != nil {
		return nil, fmt.Errorf("marshal runtime event raw map: %w", err)
	}
	items := make([]*agentruntimepb.PlanItem, 0, len(event.Items))
	for _, item := range event.Items {
		items = append(items, &agentruntimepb.PlanItem{
			Id:       item.ID,
			Step:     item.Step,
			Status:   item.Status,
			Priority: item.Priority,
		})
	}
	return &agentruntimepb.RuntimeEvent{
		Type:                    event.Type,
		EventId:                 event.EventID,
		CommandId:               event.CommandID,
		ConversationId:          event.ConversationID,
		RuntimeSessionId:        event.RuntimeSessionID,
		TurnId:                  event.TurnID,
		Delta:                   event.Delta,
		Accumulated:             event.Accumulated,
		Response:                event.Response,
		Reason:                  event.Reason,
		Message:                 event.Message,
		Items:                   items,
		ToolCallId:              event.ToolCallID,
		ToolName:                event.ToolName,
		ArgumentsJson:           argumentsJSON,
		Result:                  event.Result,
		Error:                   event.Error,
		RequestId:               event.RequestID,
		Permission:              event.Permission,
		Decision:                event.Decision,
		Summary:                 event.Summary,
		TaskId:                  event.TaskID,
		Strategy:                event.Strategy,
		InputMessageCount:       int64(event.InputMessageCount),
		InputChars:              int64(event.InputChars),
		ReplacementMessageCount: int64(event.ReplacementMessageCount),
		ArtifactPath:            event.ArtifactPath,
		RuntimeEventType:        firstNonEmpty(event.RuntimeEventType, event.Type),
		RuntimeTraceJson:        event.RuntimeTraceJSON,
		PayloadJson:             event.PayloadJSON,
		OccurredAt:              event.OccurredAt,
		Sequence:                event.Sequence,
		AssistantMessageId:      event.AssistantMessageID,
		RawJson:                 string(rawJSON),
	}, nil
}

func protoToEvent(pb *agentruntimepb.RuntimeEvent) (Event, error) {
	if pb == nil {
		return Event{}, fmt.Errorf("runtime event is nil")
	}
	var event Event
	var arguments map[string]interface{}
	if pb.ArgumentsJson != "" {
		if err := json.Unmarshal([]byte(pb.ArgumentsJson), &arguments); err != nil {
			return Event{}, fmt.Errorf("decode runtime event arguments: %w", err)
		}
	}
	var raw map[string]interface{}
	if pb.RawJson != "" {
		_ = json.Unmarshal([]byte(pb.RawJson), &raw)
		_ = json.Unmarshal([]byte(pb.RawJson), &event)
	}
	items := make([]PlanItem, 0, len(pb.Items))
	for _, item := range pb.Items {
		if item == nil {
			continue
		}
		items = append(items, PlanItem{
			ID:       item.Id,
			Step:     item.Step,
			Status:   item.Status,
			Priority: item.Priority,
		})
	}
	if pb.Type != "" {
		event.Type = pb.Type
	}
	if pb.EventId != "" {
		event.EventID = pb.EventId
	}
	if pb.CommandId != "" {
		event.CommandID = pb.CommandId
	}
	if pb.ConversationId != "" {
		event.ConversationID = pb.ConversationId
	}
	if pb.RuntimeSessionId != "" {
		event.RuntimeSessionID = pb.RuntimeSessionId
	}
	if pb.TurnId != "" {
		event.TurnID = pb.TurnId
	}
	if pb.Delta != "" {
		event.Delta = pb.Delta
	}
	if pb.Accumulated != "" {
		event.Accumulated = pb.Accumulated
	}
	if pb.Response != "" {
		event.Response = pb.Response
	}
	if pb.Reason != "" {
		event.Reason = pb.Reason
	}
	if pb.Message != "" {
		event.Message = pb.Message
	}
	if len(items) > 0 {
		event.Items = items
	}
	if pb.ToolCallId != "" {
		event.ToolCallID = pb.ToolCallId
	}
	if pb.ToolName != "" {
		event.ToolName = pb.ToolName
	}
	if arguments != nil {
		event.Arguments = arguments
	}
	if pb.Result != "" {
		event.Result = pb.Result
	}
	if pb.Error != "" {
		event.Error = pb.Error
	}
	if pb.RequestId != "" {
		event.RequestID = pb.RequestId
	}
	if pb.Permission != "" {
		event.Permission = pb.Permission
	}
	if pb.Decision != "" {
		event.Decision = pb.Decision
	}
	if pb.Summary != "" {
		event.Summary = pb.Summary
	}
	if pb.TaskId != "" {
		event.TaskID = pb.TaskId
	}
	if pb.Strategy != "" {
		event.Strategy = pb.Strategy
	}
	if pb.InputMessageCount != 0 {
		event.InputMessageCount = int(pb.InputMessageCount)
	}
	if pb.InputChars != 0 {
		event.InputChars = int(pb.InputChars)
	}
	if pb.ReplacementMessageCount != 0 {
		event.ReplacementMessageCount = int(pb.ReplacementMessageCount)
	}
	if pb.ArtifactPath != "" {
		event.ArtifactPath = pb.ArtifactPath
	}
	if pb.RuntimeEventType != "" {
		event.RuntimeEventType = pb.RuntimeEventType
	}
	if pb.RuntimeTraceJson != "" {
		event.RuntimeTraceJSON = pb.RuntimeTraceJson
	}
	if pb.PayloadJson != "" {
		event.PayloadJSON = pb.PayloadJson
	}
	if pb.OccurredAt != "" {
		event.OccurredAt = pb.OccurredAt
	}
	if pb.Sequence != "" {
		event.Sequence = pb.Sequence
	}
	if pb.AssistantMessageId != "" {
		event.AssistantMessageID = pb.AssistantMessageId
	}
	event.Raw = raw
	if event.Raw == nil {
		rawBytes, err := json.Marshal(event)
		if err != nil {
			return Event{}, fmt.Errorf("marshal runtime event fallback raw: %w", err)
		}
		_ = json.Unmarshal(rawBytes, &event.Raw)
	}
	return event, nil
}

func firstNonEmpty(values ...string) string {
	for _, value := range values {
		if value != "" {
			return value
		}
	}
	return ""
}

func marshalJSONMap(value map[string]interface{}) (string, error) {
	if value == nil {
		return "", nil
	}
	raw, err := json.Marshal(value)
	if err != nil {
		return "", err
	}
	return string(raw), nil
}
