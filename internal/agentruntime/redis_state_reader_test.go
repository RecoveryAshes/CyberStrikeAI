package agentruntime

import (
	"encoding/json"
	"testing"
)

func TestDecodeRunState(t *testing.T) {
	state, err := decodeRunState(`{
		"conversation_id":"conv-1",
		"runtime_session_id":"session-1",
		"turn_id":"turn-1",
		"status":"running",
		"message":"requesting model sample",
		"assistant_message_id":"assistant-1",
		"updated_at":"1740000000"
	}`)
	if err != nil {
		t.Fatalf("decodeRunState: %v", err)
	}
	if state.ConversationID != "conv-1" || state.Status != "running" || state.AssistantMessageID != "assistant-1" {
		t.Fatalf("state = %#v", state)
	}
}

func TestDecodeStreamEventsFromRedisRawJSON(t *testing.T) {
	rawEvent, err := json.Marshal(map[string]interface{}{
		"type":               "tool_call_started",
		"conversation_id":    "conv-1",
		"runtime_session_id": "session-1",
		"turn_id":            "turn-1",
		"tool_call_id":       "call-1",
		"tool_name":          "web_search",
		"arguments": map[string]interface{}{
			"query": "快代理 私密代理 API",
		},
		"items": []map[string]interface{}{
			{"id": "step-1", "step": "查官方文档", "status": "in_progress", "priority": "high"},
		},
	})
	if err != nil {
		t.Fatalf("marshal raw event: %v", err)
	}
	reply := []interface{}{
		[]interface{}{
			"1740000000000-0",
			[]interface{}{
				"raw_json", string(rawEvent),
				"runtime_event_type", "tool_call_started",
				"runtime_trace_json", `{"event":"tool_call_started"}`,
				"payload_json", string(rawEvent),
				"created_at_unix", "1740000000",
			},
		},
	}

	events, err := decodeStreamEvents(reply)
	if err != nil {
		t.Fatalf("decodeStreamEvents: %v", err)
	}
	if len(events) != 1 {
		t.Fatalf("events len = %d, want 1", len(events))
	}
	event := events[0]
	if event.EventID != "1740000000000-0" || event.Type != "tool_call_started" || event.ToolName != "web_search" {
		t.Fatalf("event = %#v", event)
	}
	if event.Arguments["query"] != "快代理 私密代理 API" {
		t.Fatalf("arguments = %#v", event.Arguments)
	}
	if len(event.Items) != 1 || event.Items[0].Priority != "high" {
		t.Fatalf("items = %#v", event.Items)
	}
	if event.RuntimeTraceJSON == "" || event.PayloadJSON == "" || event.Sequence != "1740000000000-0" {
		t.Fatalf("envelope = %#v", event)
	}
}
