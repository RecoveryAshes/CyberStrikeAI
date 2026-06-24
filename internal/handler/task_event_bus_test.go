package handler

import (
	"encoding/json"
	"testing"
	"time"
)

func TestTaskEventBusGlobalSubscriberGetsAllConversationEvents(t *testing.T) {
	bus := NewTaskEventBus()
	sub, ch := bus.SubscribeAll()
	defer bus.UnsubscribeAll(sub)

	bus.Publish("conv-a", []byte(`data: {"type":"progress","message":"a"}`+"\n\n"))
	bus.Publish("conv-b", []byte(`data: {"type":"progress","message":"b","data":{"conversationId":"conv-b"}}`+"\n\n"))

	first := readTaskEventLine(t, ch)
	second := readTaskEventLine(t, ch)

	if got := taskEventConversationID(t, first); got != "conv-a" {
		t.Fatalf("expected injected conversationId conv-a, got %q in %s", got, first)
	}
	if got := taskEventConversationID(t, second); got != "conv-b" {
		t.Fatalf("expected preserved conversationId conv-b, got %q in %s", got, second)
	}
}

func TestTaskEventBusCloseConversationDoesNotCloseGlobalSubscriber(t *testing.T) {
	bus := NewTaskEventBus()
	sub, ch := bus.SubscribeAll()
	defer bus.UnsubscribeAll(sub)

	bus.CloseConversation("conv-a")
	bus.Publish("conv-b", []byte(`data: {"type":"done"}`+"\n\n"))

	line := readTaskEventLine(t, ch)
	if got := taskEventConversationID(t, line); got != "conv-b" {
		t.Fatalf("expected global subscriber to remain open for conv-b, got %q in %s", got, line)
	}
}

func readTaskEventLine(t *testing.T, ch <-chan []byte) []byte {
	t.Helper()
	select {
	case line, ok := <-ch:
		if !ok {
			t.Fatal("event channel closed")
		}
		return line
	case <-time.After(time.Second):
		t.Fatal("timed out waiting for task event")
		return nil
	}
}

func taskEventConversationID(t *testing.T, line []byte) string {
	t.Helper()
	const prefix = "data: "
	if len(line) < len(prefix) || string(line[:len(prefix)]) != prefix {
		t.Fatalf("unexpected SSE line %q", line)
	}
	payload := line[len(prefix):]
	var envelope struct {
		Data map[string]interface{} `json:"data"`
	}
	if err := json.Unmarshal(payload, &envelope); err != nil {
		t.Fatalf("unmarshal event: %v; line=%s", err, line)
	}
	if envelope.Data == nil {
		return ""
	}
	if id, ok := envelope.Data["conversationId"].(string); ok {
		return id
	}
	return ""
}
