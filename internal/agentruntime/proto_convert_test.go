package agentruntime

import "testing"

func TestRuntimeCommandProtoRoundTripPreservesDynamicContext(t *testing.T) {
	cmd := Command{
		Type:             "start_turn",
		CommandID:        "cmd-1",
		ConversationID:   "conv-1",
		RuntimeSessionID: "session-1",
		Message:          "hello",
		Context: map[string]interface{}{
			"max_steps": float64(10),
			"tools": []interface{}{
				map[string]interface{}{"name": "runtime_echo", "enabled": true},
			},
		},
	}
	pb, err := commandToProto(cmd)
	if err != nil {
		t.Fatalf("commandToProto: %v", err)
	}
	got, err := protoToCommand(pb)
	if err != nil {
		t.Fatalf("protoToCommand: %v", err)
	}
	if got.Type != cmd.Type || got.CommandID != cmd.CommandID || got.ConversationID != cmd.ConversationID {
		t.Fatalf("round trip command identity = %#v", got)
	}
	if got.Context["max_steps"] != float64(10) {
		t.Fatalf("context max_steps = %#v", got.Context["max_steps"])
	}
	tools, ok := got.Context["tools"].([]interface{})
	if !ok || len(tools) != 1 {
		t.Fatalf("context tools = %#v", got.Context["tools"])
	}
}

func TestRuntimeApprovalCommandProtoRoundTripPreservesComment(t *testing.T) {
	cmd := Command{
		Type:      "approval_response",
		RequestID: "approval-1",
		Decision:  "approve",
		Message:   "ship it",
		Context: map[string]interface{}{
			"session_store_dir": "/tmp/runtime-sessions",
		},
	}
	pb, err := commandToProto(cmd)
	if err != nil {
		t.Fatalf("commandToProto: %v", err)
	}
	if pb.ApprovalMessage != "ship it" {
		t.Fatalf("approval message = %q", pb.ApprovalMessage)
	}
	got, err := protoToCommand(pb)
	if err != nil {
		t.Fatalf("protoToCommand: %v", err)
	}
	if got.Message != "ship it" || got.RequestID != "approval-1" || got.Decision != "approve" {
		t.Fatalf("round trip approval command = %#v", got)
	}
}

func TestRuntimeEventProtoRoundTripPreservesFrontendFields(t *testing.T) {
	event := Event{
		Type:             "tool_call_started",
		EventID:          "1740000000000-0",
		ConversationID:   "conv-1",
		RuntimeSessionID: "session-1",
		TurnID:           "turn-1",
		ToolCallID:       "call-1",
		ToolName:         "runtime_echo",
		Arguments: map[string]interface{}{
			"message": "hello",
		},
		Items: []PlanItem{{ID: "step-1", Step: "inspect", Status: "in_progress", Priority: "high"}},
	}
	pb, err := eventToProto(event)
	if err != nil {
		t.Fatalf("eventToProto: %v", err)
	}
	got, err := protoToEvent(pb)
	if err != nil {
		t.Fatalf("protoToEvent: %v", err)
	}
	if got.Type != event.Type || got.ToolCallID != event.ToolCallID || got.ToolName != event.ToolName {
		t.Fatalf("round trip event identity = %#v", got)
	}
	if got.EventID != event.EventID {
		t.Fatalf("event id = %q, want %q", got.EventID, event.EventID)
	}
	if got.Arguments["message"] != "hello" {
		t.Fatalf("arguments message = %#v", got.Arguments["message"])
	}
	if len(got.Items) != 1 || got.Items[0].Priority != "high" {
		t.Fatalf("items = %#v", got.Items)
	}
	if got.Raw["type"] != "tool_call_started" {
		t.Fatalf("raw type = %#v", got.Raw["type"])
	}
}

func TestRuntimeEventProtoRoundTripPreservesEnvelopeFields(t *testing.T) {
	event := Event{
		Type:               "turn_completed",
		EventID:            "1740000000000-1",
		ConversationID:     "conv-1",
		RuntimeSessionID:   "session-1",
		TurnID:             "turn-1",
		Response:           "final answer",
		RuntimeEventType:   "turn_completed",
		RuntimeTraceJSON:   `{"event":"turn_completed","response":"final answer"}`,
		PayloadJSON:        `{"type":"turn_completed","response":"final answer"}`,
		OccurredAt:         "1740000000",
		Sequence:           "1740000000000-1",
		AssistantMessageID: "assistant-1",
	}
	pb, err := eventToProto(event)
	if err != nil {
		t.Fatalf("eventToProto: %v", err)
	}
	got, err := protoToEvent(pb)
	if err != nil {
		t.Fatalf("protoToEvent: %v", err)
	}
	if got.RuntimeEventType != "turn_completed" || got.RuntimeTraceJSON == "" || got.PayloadJSON == "" {
		t.Fatalf("envelope fields = %#v", got)
	}
	if got.OccurredAt != "1740000000" || got.Sequence != "1740000000000-1" || got.AssistantMessageID != "assistant-1" {
		t.Fatalf("identity envelope fields = %#v", got)
	}
}
