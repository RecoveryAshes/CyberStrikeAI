package handler

import (
	"bytes"
	"context"
	"database/sql"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"cyberstrike-ai/internal/agent"
	"cyberstrike-ai/internal/agentruntime"
	"cyberstrike-ai/internal/config"
	"cyberstrike-ai/internal/database"
	"cyberstrike-ai/internal/mcp"
	"cyberstrike-ai/internal/openai"

	"github.com/gin-gonic/gin"
	_ "github.com/mattn/go-sqlite3"
	"go.uber.org/zap"
)

type fakeRuntimeClient struct {
	interruptConversationID string
	interruptReason         string
	interruptContinueAfter  bool
	startTurnCalls          int
}

func (f *fakeRuntimeClient) StartTurn(context.Context, agentruntime.Command, func(agentruntime.Event) error) error {
	f.startTurnCalls++
	return nil
}

func (f *fakeRuntimeClient) InterruptTurn(_ context.Context, conversationID, reason string, continueAfter bool) error {
	f.interruptConversationID = conversationID
	f.interruptReason = reason
	f.interruptContinueAfter = continueAfter
	return nil
}

func (f *fakeRuntimeClient) ResumeApproval(context.Context, agentruntime.Command, func(agentruntime.Event) error) error {
	return nil
}

func (f *fakeRuntimeClient) IsStarted() bool {
	return true
}

func (f *fakeRuntimeClient) Close() error {
	return nil
}

func TestAgentRuntimeLoopStreamPureStreamingProxyToRustAPI(t *testing.T) {
	gin.SetMode(gin.TestMode)
	db, err := database.NewDB(filepath.Join(t.TempDir(), "stream.sqlite"), zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	defer db.Close()
	conv, err := db.CreateConversation("stream", database.ConversationCreateMeta{})
	if err != nil {
		t.Fatalf("CreateConversation: %v", err)
	}

	var streamBody map[string]interface{}
	var unexpectedCalls []string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		switch {
		case r.Method == http.MethodPost && r.URL.Path == "/api/internal/messages":
			unexpectedCalls = append(unexpectedCalls, r.Method+" "+r.URL.Path)
			http.Error(w, "unexpected message sync", http.StatusInternalServerError)
		case r.Method == http.MethodPut && strings.HasPrefix(r.URL.Path, "/api/internal/agent-runtime/tasks/"):
			unexpectedCalls = append(unexpectedCalls, r.Method+" "+r.URL.Path)
			http.Error(w, "unexpected task sync", http.StatusInternalServerError)
		case r.Method == http.MethodGet && r.URL.Path == "/api/config":
			unexpectedCalls = append(unexpectedCalls, r.Method+" "+r.URL.Path)
			http.Error(w, "unexpected config read", http.StatusInternalServerError)
		case r.Method == http.MethodPost && r.URL.Path == "/api/agent-runtime/stream":
			if err := json.Unmarshal(body, &streamBody); err != nil {
				t.Fatalf("decode stream body: %v; body=%s", err, body)
			}
			w.Header().Set("Content-Type", "text/event-stream; charset=utf-8")
			_, _ = w.Write([]byte(`data: {"type":"runtime_status_update","message":"accepted by rust","data":{"conversationId":"conv-stream","runtimeEventType":"runtime_status_update","background":true,"agentMode":"agent_runtime","assistantMessageId":"assistant-rust"}}` + "\n\n" + `data: {"type":"done","message":"","data":{"conversationId":"conv-stream","background":true}}` + "\n\n"))
		case r.Method == http.MethodGet && strings.HasPrefix(r.URL.Path, "/api/internal/agent-runtime/tasks/"):
			unexpectedCalls = append(unexpectedCalls, r.Method+" "+r.URL.Path)
			http.Error(w, "unexpected task poll", http.StatusInternalServerError)
		case r.Method == http.MethodGet && r.URL.Path == "/api/internal/agent-runtime/final-response/"+conv.ID:
			unexpectedCalls = append(unexpectedCalls, r.Method+" "+r.URL.Path)
			http.Error(w, "unexpected final response read", http.StatusInternalServerError)
		case r.Method == http.MethodGet && r.URL.Path == "/api/agent-loop/task-events":
			unexpectedCalls = append(unexpectedCalls, r.Method+" "+r.URL.Path)
			http.Error(w, "unexpected task events read", http.StatusInternalServerError)
		default:
			http.NotFound(w, r)
		}
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	h := NewAgentHandler(nil, db, &config.Config{}, zap.NewNop(), filepath.Join(t.TempDir(), "config.yaml"))
	h.httpClient = upstream.Client()
	fakeClient := &fakeRuntimeClient{}
	h.agentRuntimeClientCached = fakeClient

	body := bytes.NewBufferString(`{"conversationId":"` + conv.ID + `","message":"hello","background":true}`)
	req := httptest.NewRequest(http.MethodPost, "/api/agent-runtime/stream", body)
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	c, _ := gin.CreateTestContext(w)
	c.Request = req

	h.AgentRuntimeLoopStream(c)

	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}
	resp := w.Body.String()
	if !strings.Contains(resp, "accepted by rust") || !strings.Contains(resp, `"background":true`) {
		t.Fatalf("startup response did not come from Rust SSE: %s", resp)
	}
	if streamBody["conversationId"] != conv.ID || streamBody["message"] != "hello" {
		t.Fatalf("stream body = %#v", streamBody)
	}
	if streamBody["background"] != true {
		t.Fatalf("stream background = %#v", streamBody["background"])
	}
	for _, forbidden := range []string{"agentMode", "assistantMessageId", "userMessageId", "runtimeBinaryPath", "runtimeWorkDir", "runtimeCommand"} {
		if _, ok := streamBody[forbidden]; ok {
			t.Fatalf("Go proxy must not add %s: %#v", forbidden, streamBody)
		}
	}
	if fakeClient.startTurnCalls != 0 {
		t.Fatalf("Go runtime client StartTurn calls = %d, want 0", fakeClient.startTurnCalls)
	}
	if len(unexpectedCalls) != 0 {
		t.Fatalf("Go stream proxy made orchestration calls: %#v", unexpectedCalls)
	}
	messages, err := db.GetMessages(conv.ID)
	if err != nil {
		t.Fatalf("GetMessages: %v", err)
	}
	if len(messages) != 0 {
		t.Fatalf("Go stream proxy must not create local messages, got %+v", messages)
	}
}

type fakeRuntimeStateReader struct {
	listEventsConversationID string
	listEventsAfterEventID   string
	listEventsLimit          int
	listEvents               []agentruntime.Event
	listEventsCalls          int
	getRunState              agentruntime.RunState
	getRunStateFound         bool
	listRunStates            []agentruntime.RunState
}

func (f *fakeRuntimeStateReader) GetRunState(_ context.Context, conversationID string) (agentruntime.RunState, bool, error) {
	if f.getRunStateFound {
		f.getRunState.ConversationID = conversationID
		return f.getRunState, true, nil
	}
	for _, state := range f.listRunStates {
		if state.ConversationID == conversationID {
			return state, true, nil
		}
	}
	return agentruntime.RunState{}, false, nil
}

func (f *fakeRuntimeStateReader) ListRunStates(context.Context) ([]agentruntime.RunState, error) {
	return f.listRunStates, nil
}

func (f *fakeRuntimeStateReader) ListEvents(_ context.Context, conversationID, afterEventID string, limit int) ([]agentruntime.Event, error) {
	f.listEventsConversationID = conversationID
	f.listEventsAfterEventID = afterEventID
	f.listEventsLimit = limit
	f.listEventsCalls++
	return f.listEvents, nil
}

func TestCancelAgentRuntimeContinueAfterIsRejected(t *testing.T) {
	gin.SetMode(gin.TestMode)
	tasks := NewAgentTaskManager()
	ctx, cancel := context.WithCancelCause(context.Background())
	defer cancel(nil)
	if _, err := tasks.StartTask("conv-runtime", "running", cancel); err != nil {
		t.Fatalf("StartTask: %v", err)
	}
	tasks.SetTaskAgentMode("conv-runtime", "agent_runtime")
	h := &AgentHandler{tasks: tasks, logger: zap.NewNop()}
	body := bytes.NewBufferString(`{"conversationId":"conv-runtime","continueAfter":true,"reason":"more context"}`)
	req := httptest.NewRequest(http.MethodPost, "/api/agent-loop/cancel", body)
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	c, _ := gin.CreateTestContext(w)
	c.Request = req

	h.CancelAgentLoop(c)

	if w.Code != http.StatusConflict {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}
	if ctx.Err() != nil {
		t.Fatalf("agent runtime continueAfter rejection should not cancel task: %v", ctx.Err())
	}
	if !strings.Contains(w.Body.String(), "unsupported_continue_after") {
		t.Fatalf("response body = %s", w.Body.String())
	}
}

func TestCancelAgentRuntimePureProxyToRustCancel(t *testing.T) {
	gin.SetMode(gin.TestMode)
	var seen []string
	var cancelBody map[string]interface{}
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seen = append(seen, r.Method+" "+r.URL.Path)
		var body map[string]interface{}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			t.Fatalf("decode body: %v", err)
		}
		w.Header().Set("Content-Type", "application/json")
		if r.URL.Path == "/api/agent-loop/cancel" {
			cancelBody = body
			_, _ = w.Write([]byte(`{"status":"cancelling","conversationId":"conv-runtime","message":"from rust","continueAfter":false,"interruptWithNote":false,"agentMode":"agent_runtime"}`))
			return
		}
		_, _ = w.Write([]byte(`{"ok":true}`))
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	tasks := NewAgentTaskManager()
	h := &AgentHandler{tasks: tasks, logger: zap.NewNop(), httpClient: upstream.Client()}
	body := bytes.NewBufferString(`{"conversationId":"conv-runtime"}`)
	req := httptest.NewRequest(http.MethodPost, "/api/agent-loop/cancel", body)
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	c, _ := gin.CreateTestContext(w)
	c.Request = req

	h.CancelAgentLoop(c)

	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}
	if len(seen) != 1 || seen[0] != "POST /api/agent-loop/cancel" {
		t.Fatalf("seen = %#v", seen)
	}
	if cancelBody["conversationId"] != "conv-runtime" {
		t.Fatalf("cancel body = %#v", cancelBody)
	}
	if !strings.Contains(w.Body.String(), "from rust") {
		t.Fatalf("response body = %s", w.Body.String())
	}
	if task := tasks.GetTask("conv-runtime"); task != nil {
		t.Fatalf("Go cancel proxy must not mutate local task state: %#v", task)
	}
}

func TestCancelAgentRuntimePropagatesRustNotFound(t *testing.T) {
	gin.SetMode(gin.TestMode)
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Error(w, `{"error":"未找到正在执行的任务"}`, http.StatusNotFound)
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	tasks := NewAgentTaskManager()
	h := &AgentHandler{tasks: tasks, logger: zap.NewNop(), httpClient: upstream.Client()}
	body := bytes.NewBufferString(`{"conversationId":"conv-local","reason":"stop"}`)
	req := httptest.NewRequest(http.MethodPost, "/api/agent-loop/cancel", body)
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	c, _ := gin.CreateTestContext(w)
	c.Request = req

	h.CancelAgentLoop(c)

	if w.Code != http.StatusNotFound {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}
	if !strings.Contains(w.Body.String(), "未找到正在执行的任务") {
		t.Fatalf("response body = %s", w.Body.String())
	}
}

func TestListAgentTasksProxiesToRustAPI(t *testing.T) {
	gin.SetMode(gin.TestMode)
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodGet || r.URL.Path != "/api/agent-loop/tasks" {
			http.NotFound(w, r)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"tasks":[{"conversationId":"conv-pg","message":"from pg","startedAt":"2026-06-25 20:00:00+00","status":"running","agentMode":"agent_runtime","assistantMessageId":"assistant-pg"}]}`))
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	h := &AgentHandler{tasks: NewAgentTaskManager(), logger: zap.NewNop(), httpClient: upstream.Client()}
	req := httptest.NewRequest(http.MethodGet, "/api/agent-loop/tasks", nil)
	w := httptest.NewRecorder()
	c, _ := gin.CreateTestContext(w)
	c.Request = req

	h.ListAgentTasks(c)

	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}
	if strings.TrimSpace(w.Body.String()) != `{"tasks":[{"conversationId":"conv-pg","message":"from pg","startedAt":"2026-06-25 20:00:00+00","status":"running","agentMode":"agent_runtime","assistantMessageId":"assistant-pg"}]}` {
		t.Fatalf("unexpected body: %s", w.Body.String())
	}
}

func TestMergeAgentRuntimeRunStates(t *testing.T) {
	started := time.Now()
	tasks := []*AgentTask{{
		ConversationID: "conv-1",
		Message:        "local",
		StartedAt:      started,
		Status:         "running",
	}}
	states := []agentruntime.RunState{
		{ConversationID: "conv-1", Status: "awaiting_approval", Message: "approval", AssistantMessageID: "assistant-1"},
		{ConversationID: "conv-2", Status: "running", Message: "remote", AssistantMessageID: "assistant-2"},
	}

	merged := mergeAgentRuntimeRunStates(tasks, states)

	if len(merged) != 2 {
		t.Fatalf("merged len = %d, want 2: %#v", len(merged), merged)
	}
	if merged[0].ConversationID != "conv-1" || merged[0].Status != "awaiting_approval" || merged[0].AgentMode != "agent_runtime" || merged[0].AssistantMessageID != "assistant-1" {
		t.Fatalf("merged existing task = %#v", merged[0])
	}
	if merged[1].ConversationID != "conv-2" || merged[1].Status != "running" || merged[1].AgentMode != "agent_runtime" || merged[1].AssistantMessageID != "assistant-2" {
		t.Fatalf("merged runtime task = %#v", merged[1])
	}
}

func TestAgentRuntimeReplayMapsEventsWithoutSideEffects(t *testing.T) {
	h := &AgentHandler{}
	line, ok := h.agentRuntimeReplayEventLine(agentruntime.Event{
		Type:             "approval_requested",
		EventID:          "1740000000000-0",
		ConversationID:   "conv-1",
		RuntimeSessionID: "session-1",
		TurnID:           "turn-1",
		RequestID:        "approval-1",
		Permission:       "mcp_call",
		ToolCallID:       "call-1",
		ToolName:         "mcp_call",
		Message:          "approve tool",
	})
	if !ok {
		t.Fatalf("expected replay line")
	}
	var envelope StreamEvent
	raw := strings.TrimSpace(strings.TrimPrefix(strings.TrimSpace(string(line)), "data:"))
	if err := json.Unmarshal([]byte(raw), &envelope); err != nil {
		t.Fatalf("decode replay line: %v", err)
	}
	if envelope.Type != "hitl_approval_requested" || envelope.Message != "approve tool" {
		t.Fatalf("envelope = %#v", envelope)
	}
	data, ok := envelope.Data.(map[string]interface{})
	if !ok {
		t.Fatalf("data = %#v", envelope.Data)
	}
	if data["replay"] != true || data["runtimeEventId"] != "1740000000000-0" || data["agentMode"] != "agent_runtime" {
		t.Fatalf("data = %#v", data)
	}
	if data["interruptId"] != nil {
		t.Fatalf("replay must not synthesize HITL interrupt id: %#v", data)
	}
}

func TestAgentRuntimeReplayAssistantDeltaUsesAccumulatedFromEvent(t *testing.T) {
	h := &AgentHandler{}
	ev := h.agentRuntimeReplayStreamEvent(agentruntime.Event{
		Type:             "assistant_delta",
		ConversationID:   "conv-1",
		RuntimeSessionID: "session-1",
		TurnID:           "turn-1",
		Delta:            "lo",
		Accumulated:      "hello",
	})
	if ev.Type != "response_delta" || ev.Message != "lo" {
		t.Fatalf("stream event = %#v", ev)
	}
	data, ok := ev.Data.(map[string]interface{})
	if !ok {
		t.Fatalf("data = %#v", ev.Data)
	}
	if data[openai.SSEAccumulatedKey] != "hello" {
		t.Fatalf("accumulated = %#v", data[openai.SSEAccumulatedKey])
	}
}

func TestAgentRuntimeReplayTurnCompletedIncludesResponseAndDone(t *testing.T) {
	h := &AgentHandler{}
	lines := h.agentRuntimeReplayEventLines(agentruntime.Event{
		Type:               "turn_completed",
		EventID:            "5-0",
		ConversationID:     "conv-1",
		RuntimeSessionID:   "session-1",
		TurnID:             "turn-1",
		Response:           "final answer",
		AssistantMessageID: "assistant-1",
	})
	if len(lines) != 3 {
		t.Fatalf("lines len = %d, want 3: %#v", len(lines), lines)
	}
	body := string(bytes.Join(lines, nil))
	if !strings.Contains(body, `"type":"response"`) || !strings.Contains(body, `"message":"final answer"`) {
		t.Fatalf("replay response missing: %s", body)
	}
	if !strings.Contains(body, `"type":"done"`) {
		t.Fatalf("replay done missing: %s", body)
	}
	if !strings.Contains(body, `"assistantMessageId":"assistant-1"`) {
		t.Fatalf("assistant message id missing: %s", body)
	}
}

func TestSubscribeAgentTaskEventsProxiesRustSSE(t *testing.T) {
	gin.SetMode(gin.TestMode)
	var seenPath, seenQuery, seenLastEventID string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seenPath = r.URL.Path
		seenQuery = r.URL.RawQuery
		seenLastEventID = r.Header.Get("Last-Event-ID")
		w.Header().Set("Content-Type", "text/event-stream; charset=utf-8")
		_, _ = w.Write([]byte("id: 42\n"))
		_, _ = w.Write([]byte(`data: {"type":"assistant_progress_update","data":{"conversationId":"conv-1","runtimeEventId":"2-0"}}` + "\n\n"))
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	h := &AgentHandler{
		config:     &config.Config{},
		tasks:      NewAgentTaskManager(),
		httpClient: upstream.Client(),
		logger:     zap.NewNop(),
	}

	req := httptest.NewRequest(http.MethodGet, "/api/agent-loop/task-events?conversationId=conv-1&afterEventId=1-0&limit=7", nil)
	req.Header.Set("Last-Event-ID", "1")
	w := httptest.NewRecorder()
	c, _ := gin.CreateTestContext(w)
	c.Request = req

	h.SubscribeAgentTaskEvents(c)

	if seenPath != "/api/agent-loop/task-events" || !strings.Contains(seenQuery, "conversationId=conv-1") || !strings.Contains(seenQuery, "afterEventId=1-0") || !strings.Contains(seenQuery, "limit=7") {
		t.Fatalf("proxied request path=%q query=%q", seenPath, seenQuery)
	}
	if seenLastEventID != "1" {
		t.Fatalf("Last-Event-ID = %q, want 1", seenLastEventID)
	}
	if w.Code != http.StatusOK || !strings.Contains(w.Body.String(), `"runtimeEventId":"2-0"`) {
		t.Fatalf("status=%d body=%s", w.Code, w.Body.String())
	}
	if contentType := w.Header().Get("Content-Type"); !strings.Contains(contentType, "text/event-stream") {
		t.Fatalf("Content-Type = %q", contentType)
	}
}

func TestAgentRuntimeTaskEventBridgeFlushesTerminalAfterLegacyClose(t *testing.T) {
	reader := &fakeRuntimeStateReader{
		listRunStates: []agentruntime.RunState{{
			ConversationID:     "conv-terminal",
			Status:             "running",
			AssistantMessageID: "assistant-terminal",
		}},
		listEvents: []agentruntime.Event{{
			Type:               "turn_completed",
			EventID:            "9-0",
			ConversationID:     "conv-terminal",
			RuntimeSessionID:   "session-1",
			TurnID:             "turn-1",
			Response:           "done from redis",
			AssistantMessageID: "assistant-terminal",
		}},
	}
	h := &AgentHandler{
		config: &config.Config{
			AgentRuntime: config.AgentRuntimeConfig{Enabled: true, Transport: "grpc", RedisAddr: "127.0.0.1:6379"},
		},
		tasks:                         NewAgentTaskManager(),
		taskEventBus:                  NewTaskEventBus(),
		agentRuntimeStateReaderCached: reader,
		agentRuntimeStateRedisAddr:    "127.0.0.1:6379",
		agentRuntimeStateRedisPrefix:  "csai:agent_runtime:",
	}
	bridge := h.newAgentRuntimeTaskEventBridge("", "", 100)
	legacy := make(chan []byte)
	close(legacy)
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	var lines []string
	bridge.Stream(ctx, legacy, func(line []byte) bool {
		lines = append(lines, string(line))
		return true
	})
	body := strings.Join(lines, "")
	if !strings.Contains(body, `"type":"response"`) || !strings.Contains(body, "done from redis") {
		t.Fatalf("terminal response not flushed after legacy close: %s", body)
	}
	if !strings.Contains(body, `"type":"done"`) {
		t.Fatalf("terminal done not flushed after legacy close: %s", body)
	}
}

func TestMirrorAgentRuntimeRedisEventsToRust(t *testing.T) {
	reader := &fakeRuntimeStateReader{
		listRunStates: []agentruntime.RunState{{
			ConversationID:     "conv-live",
			Status:             "running",
			Message:            "running",
			AssistantMessageID: "assistant-live",
		}},
		listEvents: []agentruntime.Event{{
			Type:             "assistant_progress_update",
			EventID:          "3-0",
			ConversationID:   "conv-live",
			RuntimeSessionID: "session-1",
			TurnID:           "turn-1",
			Message:          "正在打开官方文档。",
		}},
	}
	posted := make(chan string, 1)
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.URL.Path != "/api/internal/agent-runtime/task-events" {
			http.NotFound(w, r)
			return
		}
		body, _ := io.ReadAll(r.Body)
		posted <- string(body)
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"ok":true,"id":1}`))
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	tasks := NewAgentTaskManager()
	_, err := tasks.StartTask("conv-live", "查询快代理官方文档，给我私密代理的参数和api", func(error) {})
	if err != nil {
		t.Fatalf("StartTask: %v", err)
	}
	tasks.SetTaskAgentMode("conv-live", "agent_runtime")
	h := &AgentHandler{
		config: &config.Config{
			AgentRuntime: config.AgentRuntimeConfig{Enabled: true, Transport: "grpc", RedisAddr: "127.0.0.1:6379"},
		},
		tasks:                         tasks,
		taskEventBus:                  NewTaskEventBus(),
		agentRuntimeStateReaderCached: reader,
		agentRuntimeStateRedisAddr:    "127.0.0.1:6379",
		agentRuntimeStateRedisPrefix:  "csai:agent_runtime:",
		httpClient:                    upstream.Client(),
		logger:                        zap.NewNop(),
	}

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go h.mirrorAgentRuntimeRedisEventsToRust(ctx, "", "", 100)

	var body string
	select {
	case body = <-posted:
	case <-time.After(time.Second):
		t.Fatal("timed out waiting for Rust task-event sync")
	}
	var payload struct {
		ConversationID string `json:"conversationId"`
		EventType      string `json:"eventType"`
		Line           string `json:"line"`
		RuntimeEventID string `json:"runtimeEventId"`
	}
	if err := json.Unmarshal([]byte(body), &payload); err != nil {
		t.Fatalf("decode sync body: %v; body=%s", err, body)
	}
	if payload.ConversationID != "conv-live" {
		t.Fatalf("conversationId = %q, want conv-live; body=%s", payload.ConversationID, body)
	}
	if payload.EventType != "assistant_progress_update" || payload.RuntimeEventID != "3-0" {
		t.Fatalf("event identity = (%q, %q), want assistant_progress_update/3-0; body=%s", payload.EventType, payload.RuntimeEventID, body)
	}
	if !strings.Contains(payload.Line, `"runtimeEventType":"assistant_progress_update"`) {
		t.Fatalf("runtime event did not preserve frontend envelope contract in line: %s", payload.Line)
	}
	if !strings.Contains(payload.Line, `"assistantMessageId":"assistant-live"`) {
		t.Fatalf("runtime event did not attach assistant message id from runtime state: %s", payload.Line)
	}
	if reader.listEventsConversationID != "conv-live" {
		t.Fatalf("ListEvents conversation = %q, want conv-live", reader.listEventsConversationID)
	}
	if reader.listEventsCalls == 0 {
		t.Fatal("expected ListEvents to be called")
	}
}

func TestAgentRuntimeTaskEventBridgeUsesRunStatesWithoutGoTask(t *testing.T) {
	reader := &fakeRuntimeStateReader{
		listRunStates: []agentruntime.RunState{{
			ConversationID:     "conv-redis",
			Status:             "running",
			Message:            "running from redis",
			AssistantMessageID: "assistant-redis",
		}},
		listEvents: []agentruntime.Event{{
			Type:           "tool_call_started",
			EventID:        "4-0",
			ConversationID: "conv-redis",
			TurnID:         "turn-1",
			ToolCallID:     "call-1",
			ToolName:       "web_search",
		}},
	}
	h := &AgentHandler{
		config: &config.Config{
			AgentRuntime: config.AgentRuntimeConfig{Enabled: true, Transport: "grpc", RedisAddr: "127.0.0.1:6379"},
		},
		tasks:                         NewAgentTaskManager(),
		agentRuntimeStateReaderCached: reader,
		agentRuntimeStateRedisAddr:    "127.0.0.1:6379",
		agentRuntimeStateRedisPrefix:  "csai:agent_runtime:",
	}
	bridge := h.newAgentRuntimeTaskEventBridge("", "", 100)

	var lines []string
	ok := bridge.flushRuntimeEvents(context.Background(), func(line []byte) bool {
		lines = append(lines, string(line))
		return true
	})

	if !ok {
		t.Fatal("flushRuntimeEvents returned false")
	}
	if len(lines) != 1 {
		t.Fatalf("lines len = %d, want 1: %#v", len(lines), lines)
	}
	line := lines[0]
	if !strings.Contains(line, `"conversationId":"conv-redis"`) || !strings.Contains(line, `"runtimeEventId":"4-0"`) {
		t.Fatalf("line does not contain bridged runtime identity: %s", line)
	}
	if !strings.Contains(line, `"assistantMessageId":"assistant-redis"`) {
		t.Fatalf("line does not contain assistant message id from runtime state: %s", line)
	}
	if !strings.Contains(line, `"runtimeEventType":"tool_call_started"`) {
		t.Fatalf("line does not preserve runtime event type: %s", line)
	}
	if reader.listEventsConversationID != "conv-redis" {
		t.Fatalf("ListEvents conversation = %q, want conv-redis", reader.listEventsConversationID)
	}
}

func TestInterruptAgentRuntimeRunDoesNotRequireListStatePreflight(t *testing.T) {
	client := &fakeRuntimeClient{}
	h := &AgentHandler{
		config: &config.Config{
			AgentRuntime: config.AgentRuntimeConfig{Enabled: true, Transport: "grpc"},
		},
		agentRuntimeClientCached: client,
	}

	ok := h.interruptAgentRuntimeRunFromHTTP(context.Background(), "conv-1", "stop", false)

	if !ok {
		t.Fatal("expected interrupt to be attempted without run-state preflight")
	}
	if client.interruptConversationID != "conv-1" || client.interruptReason != "stop" || client.interruptContinueAfter {
		t.Fatalf("interrupt args = conversation %q reason %q continue %v", client.interruptConversationID, client.interruptReason, client.interruptContinueAfter)
	}
}

func TestHandleAgentRuntimeEventCompletionGate(t *testing.T) {
	h := &AgentHandler{}
	var finalResponse strings.Builder
	var reasoning strings.Builder
	var completed bool
	var aborted bool
	var events []string
	progress := func(eventType, message string, data interface{}) {
		events = append(events, eventType)
	}

	err := h.handleAgentRuntimeEvent(progress, "conv-1", "", agentruntime.Event{
		Type:             "assistant_delta",
		ConversationID:   "conv-1",
		RuntimeSessionID: "session-1",
		TurnID:           "turn-1",
		Delta:            "partial",
		Accumulated:      "partial",
	}, &finalResponse, &reasoning, &completed, &aborted, new(bool))
	if err != nil {
		t.Fatalf("assistant_delta handler: %v", err)
	}
	if completed || aborted {
		t.Fatalf("assistant_delta should not complete or abort: completed=%v aborted=%v", completed, aborted)
	}

	err = h.handleAgentRuntimeEvent(progress, "conv-1", "", agentruntime.Event{
		Type:             "turn_completed",
		ConversationID:   "conv-1",
		RuntimeSessionID: "session-1",
		TurnID:           "turn-1",
		Response:         "final",
	}, &finalResponse, &reasoning, &completed, &aborted, new(bool))
	if err != nil {
		t.Fatalf("turn_completed handler: %v", err)
	}
	if !completed || aborted {
		t.Fatalf("turn_completed should complete only: completed=%v aborted=%v", completed, aborted)
	}
	if got := finalResponse.String(); got != "final" {
		t.Fatalf("final response = %q, want final", got)
	}
	if len(events) != 2 || events[0] != "response_delta" || events[1] != "progress" {
		t.Fatalf("unexpected progress events: %#v", events)
	}
}

func TestHandleAgentRuntimeEventAbortDoesNotComplete(t *testing.T) {
	h := &AgentHandler{}
	var finalResponse strings.Builder
	var reasoning strings.Builder
	var completed bool
	var aborted bool

	err := h.handleAgentRuntimeEvent(func(string, string, interface{}) {}, "conv-1", "", agentruntime.Event{
		Type:             "turn_aborted",
		ConversationID:   "conv-1",
		RuntimeSessionID: "session-1",
		TurnID:           "turn-1",
		Reason:           "cancelled",
	}, &finalResponse, &reasoning, &completed, &aborted, new(bool))
	if err != nil {
		t.Fatalf("turn_aborted handler: %v", err)
	}
	if completed || !aborted {
		t.Fatalf("turn_aborted should abort only: completed=%v aborted=%v", completed, aborted)
	}
}

func TestAgentRuntimeReasoningDeltaPersistsProcessDetail(t *testing.T) {
	db, err := database.NewDB(filepath.Join(t.TempDir(), "reasoning.sqlite"), zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	defer db.Close()

	conv, err := db.CreateConversation("reasoning", database.ConversationCreateMeta{})
	if err != nil {
		t.Fatalf("CreateConversation: %v", err)
	}
	msg, err := db.AddMessage(conv.ID, "assistant", "处理中...", nil)
	if err != nil {
		t.Fatalf("AddMessage: %v", err)
	}
	h := &AgentHandler{db: db, logger: zap.NewNop()}
	progress := h.createProgressCallback(context.Background(), func(error) {}, conv.ID, msg.ID, nil)
	var finalResponse strings.Builder
	var reasoning strings.Builder
	var completed bool
	var aborted bool
	var pendingApproval bool

	if err := h.handleAgentRuntimeEvent(progress, conv.ID, msg.ID, agentruntime.Event{
		Type:             "reasoning_delta",
		ConversationID:   conv.ID,
		RuntimeSessionID: "session-1",
		TurnID:           "turn-1",
		Delta:            "step one",
	}, &finalResponse, &reasoning, &completed, &aborted, &pendingApproval); err != nil {
		t.Fatalf("reasoning_delta handler: %v", err)
	}
	progress("done", "", map[string]interface{}{"conversationId": conv.ID})

	details, err := db.GetProcessDetails(msg.ID)
	if err != nil {
		t.Fatalf("GetProcessDetails: %v", err)
	}
	var found bool
	for _, detail := range details {
		if detail.EventType == "reasoning_chain" && detail.Message == "step one" {
			found = true
		}
	}
	if !found {
		t.Fatalf("expected reasoning_chain process detail, got %+v", details)
	}
	if got := reasoning.String(); got != "step one" {
		t.Fatalf("reasoning = %q, want step one", got)
	}
}

func TestAgentRuntimeAssistantProgressUpdatePersistsProcessDetail(t *testing.T) {
	db, err := database.NewDB(filepath.Join(t.TempDir(), "assistant-progress.sqlite"), zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	defer db.Close()

	conv, err := db.CreateConversation("assistant progress", database.ConversationCreateMeta{})
	if err != nil {
		t.Fatalf("CreateConversation: %v", err)
	}
	msg, err := db.AddMessage(conv.ID, "assistant", "处理中...", nil)
	if err != nil {
		t.Fatalf("AddMessage: %v", err)
	}
	h := &AgentHandler{db: db, logger: zap.NewNop()}
	progress := h.createProgressCallback(context.Background(), func(error) {}, conv.ID, msg.ID, nil)
	var finalResponse strings.Builder
	var reasoning strings.Builder
	var completed bool
	var aborted bool
	var pendingApproval bool

	if err := h.handleAgentRuntimeEvent(progress, conv.ID, msg.ID, agentruntime.Event{
		Type:             "assistant_progress_update",
		ConversationID:   conv.ID,
		RuntimeSessionID: "session-1",
		TurnID:           "turn-1",
		Message:          "正在检查本机进程。",
	}, &finalResponse, &reasoning, &completed, &aborted, &pendingApproval); err != nil {
		t.Fatalf("assistant_progress_update handler: %v", err)
	}
	progress("done", "", map[string]interface{}{"conversationId": conv.ID})

	details, err := db.GetProcessDetails(msg.ID)
	if err != nil {
		t.Fatalf("GetProcessDetails: %v", err)
	}
	var found bool
	for _, detail := range details {
		if detail.EventType == "assistant_progress_update" && detail.Message == "正在检查本机进程。" {
			var data map[string]interface{}
			if err := json.Unmarshal([]byte(detail.Data), &data); err != nil {
				t.Fatalf("unmarshal assistant_progress_update data: %v; raw=%s", err, detail.Data)
			}
			if data["assistantMessageId"] != msg.ID {
				t.Fatalf("assistantMessageId = %v, want %s", data["assistantMessageId"], msg.ID)
			}
			if data["turnId"] != "turn-1" {
				t.Fatalf("turnId = %v, want turn-1", data["turnId"])
			}
			trace, _ := data["runtimeTrace"].(map[string]interface{})
			if trace["event"] != "assistant_progress_update" || trace["message"] != "正在检查本机进程。" {
				t.Fatalf("unexpected runtimeTrace: %#v", trace)
			}
			found = true
		}
		if detail.EventType == "reasoning_chain" && detail.Message == "正在检查本机进程。" {
			t.Fatalf("assistant progress update must not be persisted as reasoning_chain: %+v", detail)
		}
	}
	if !found {
		t.Fatalf("expected assistant_progress_update process detail, got %+v", details)
	}
	if got := strings.TrimSpace(reasoning.String()); got != "" {
		t.Fatalf("assistant progress update must not mutate reasoning buffer, got %q", got)
	}
}

func TestAgentRuntimeStatusUpdatePersistsAsRuntimeStatusOnly(t *testing.T) {
	db, err := database.NewDB(filepath.Join(t.TempDir(), "runtime-status.sqlite"), zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	defer db.Close()

	conv, err := db.CreateConversation("runtime status", database.ConversationCreateMeta{})
	if err != nil {
		t.Fatalf("CreateConversation: %v", err)
	}
	msg, err := db.AddMessage(conv.ID, "assistant", "处理中...", nil)
	if err != nil {
		t.Fatalf("AddMessage: %v", err)
	}
	h := &AgentHandler{db: db, logger: zap.NewNop()}
	progress := h.createProgressCallback(context.Background(), func(error) {}, conv.ID, msg.ID, nil)
	var finalResponse strings.Builder
	var reasoning strings.Builder
	var completed bool
	var aborted bool
	var pendingApproval bool

	if err := h.handleAgentRuntimeEvent(progress, conv.ID, msg.ID, agentruntime.Event{
		Type:             "runtime_status_update",
		ConversationID:   conv.ID,
		RuntimeSessionID: "session-1",
		TurnID:           "turn-1",
		Message:          "工具结果已写回上下文，准备继续采样。",
	}, &finalResponse, &reasoning, &completed, &aborted, &pendingApproval); err != nil {
		t.Fatalf("runtime_status_update handler: %v", err)
	}
	progress("done", "", map[string]interface{}{"conversationId": conv.ID})

	details, err := db.GetProcessDetails(msg.ID)
	if err != nil {
		t.Fatalf("GetProcessDetails: %v", err)
	}
	var found bool
	for _, detail := range details {
		if detail.EventType == "runtime_status_update" && detail.Message == "工具结果已写回上下文，准备继续采样。" {
			var data map[string]interface{}
			if err := json.Unmarshal([]byte(detail.Data), &data); err != nil {
				t.Fatalf("unmarshal runtime_status_update data: %v; raw=%s", err, detail.Data)
			}
			trace, _ := data["runtimeTrace"].(map[string]interface{})
			if trace["event"] != "runtime_status_update" || trace["message"] != "工具结果已写回上下文，准备继续采样。" {
				t.Fatalf("unexpected runtimeTrace: %#v", trace)
			}
			found = true
		}
		if detail.EventType == "assistant_progress_update" && detail.Message == "工具结果已写回上下文，准备继续采样。" {
			t.Fatalf("runtime status update must not be persisted as assistant_progress_update: %+v", detail)
		}
		if detail.EventType == "reasoning_chain" && detail.Message == "工具结果已写回上下文，准备继续采样。" {
			t.Fatalf("runtime status update must not be persisted as reasoning_chain: %+v", detail)
		}
	}
	if !found {
		t.Fatalf("expected runtime_status_update process detail, got %+v", details)
	}
	if got := strings.TrimSpace(reasoning.String()); got != "" {
		t.Fatalf("runtime status update must not mutate reasoning buffer, got %q", got)
	}
}

func TestAgentRuntimeToolCallDeltaPersistsProcessDetail(t *testing.T) {
	db, err := database.NewDB(filepath.Join(t.TempDir(), "tool-delta.sqlite"), zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	defer db.Close()

	conv, err := db.CreateConversation("tool delta", database.ConversationCreateMeta{})
	if err != nil {
		t.Fatalf("CreateConversation: %v", err)
	}
	msg, err := db.AddMessage(conv.ID, "assistant", "处理中...", nil)
	if err != nil {
		t.Fatalf("AddMessage: %v", err)
	}
	h := &AgentHandler{db: db, logger: zap.NewNop()}
	progress := h.createProgressCallback(context.Background(), func(error) {}, conv.ID, msg.ID, nil)
	var finalResponse strings.Builder
	var reasoning strings.Builder
	var completed bool
	var aborted bool
	var pendingApproval bool

	if err := h.handleAgentRuntimeEvent(progress, conv.ID, msg.ID, agentruntime.Event{
		Type:             "tool_call_started",
		ConversationID:   conv.ID,
		RuntimeSessionID: "session-1",
		TurnID:           "turn-1",
		ToolCallID:       "call-1",
		ToolName:         "execute",
		Arguments:        map[string]interface{}{"command": "printf first"},
	}, &finalResponse, &reasoning, &completed, &aborted, &pendingApproval); err != nil {
		t.Fatalf("tool_call_started handler: %v", err)
	}
	if err := h.handleAgentRuntimeEvent(progress, conv.ID, msg.ID, agentruntime.Event{
		Type:             "tool_call_delta",
		ConversationID:   conv.ID,
		RuntimeSessionID: "session-1",
		TurnID:           "turn-1",
		ToolCallID:       "call-1",
		Delta:            "first\n",
	}, &finalResponse, &reasoning, &completed, &aborted, &pendingApproval); err != nil {
		t.Fatalf("tool_call_delta handler: %v", err)
	}
	progress("done", "", map[string]interface{}{"conversationId": conv.ID})

	details, err := db.GetProcessDetails(msg.ID)
	if err != nil {
		t.Fatalf("GetProcessDetails: %v", err)
	}
	var found bool
	for _, detail := range details {
		if detail.EventType != "tool_result_delta" {
			continue
		}
		if detail.Message != "first\n" {
			t.Fatalf("tool_result_delta message = %q, want first\\n", detail.Message)
		}
		var data map[string]interface{}
		if err := json.Unmarshal([]byte(detail.Data), &data); err != nil {
			t.Fatalf("unmarshal tool_result_delta data: %v; raw=%s", err, detail.Data)
		}
		if data["toolCallId"] != "call-1" {
			t.Fatalf("toolCallId = %v, want call-1", data["toolCallId"])
		}
		if data["delta"] != "first\n" {
			t.Fatalf("delta = %v, want first\\n", data["delta"])
		}
		trace, _ := data["runtimeTrace"].(map[string]interface{})
		if trace["event"] != "tool_call_delta" || trace["delta"] != "first\n" {
			t.Fatalf("unexpected runtimeTrace: %#v", trace)
		}
		found = true
	}
	if !found {
		t.Fatalf("expected tool_result_delta process detail, got %+v", details)
	}
}

func TestAgentRuntimeBatchQueueDoesNotFallbackWhenRuntimeDisabled(t *testing.T) {
	db, err := database.NewDB(filepath.Join(t.TempDir(), "batch.sqlite"), zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	defer db.Close()

	h := NewAgentHandler(nil, db, &config.Config{}, zap.NewNop(), filepath.Join(t.TempDir(), "config.yaml"))
	queue, err := h.batchTaskManager.CreateBatchQueue("runtime queue", "", "agent_runtime", "manual", "", "", nil, []string{"hello"})
	if err != nil {
		t.Fatalf("CreateBatchQueue: %v", err)
	}
	if queue.AgentMode != "agent_runtime" {
		t.Fatalf("queue.AgentMode = %q, want agent_runtime", queue.AgentMode)
	}

	ok, err := h.startBatchQueueExecution(queue.ID, false)
	if !ok {
		t.Fatalf("startBatchQueueExecution should find queue")
	}
	if err == nil || !strings.Contains(err.Error(), "agent_runtime") {
		t.Fatalf("startBatchQueueExecution error = %v, want agent_runtime disabled error", err)
	}
	refreshed, exists := h.batchTaskManager.GetBatchQueue(queue.ID)
	if !exists {
		t.Fatalf("queue disappeared")
	}
	if refreshed.AgentMode != "agent_runtime" {
		t.Fatalf("refreshed.AgentMode = %q, want agent_runtime", refreshed.AgentMode)
	}
}

func TestAgentRuntimeWorkDirRelativeToConfig(t *testing.T) {
	h := &AgentHandler{
		config:     &config.Config{AgentRuntime: config.AgentRuntimeConfig{WorkspaceRoot: "runtime-workspace"}},
		configPath: filepath.Join("/tmp", "cyberstrike", "config.yaml"),
	}
	want := filepath.Join("/tmp", "cyberstrike", "runtime-workspace")
	if got := h.agentRuntimeWorkDir(); got != want {
		t.Fatalf("agentRuntimeWorkDir = %q, want %q", got, want)
	}
}

func TestAgentRuntimeWorkDirDefaultsToAbsoluteCurrentDirectory(t *testing.T) {
	cwd, err := os.Getwd()
	if err != nil {
		t.Fatalf("Getwd: %v", err)
	}
	absCwd, err := filepath.Abs(cwd)
	if err != nil {
		t.Fatalf("Abs: %v", err)
	}
	for _, tc := range []struct {
		name       string
		configPath string
	}{
		{name: "relative_config", configPath: "config.yaml"},
		{name: "empty_config", configPath: ""},
	} {
		t.Run(tc.name, func(t *testing.T) {
			h := &AgentHandler{
				config:     &config.Config{},
				configPath: tc.configPath,
			}
			if got := h.agentRuntimeWorkDir(); got != absCwd {
				t.Fatalf("agentRuntimeWorkDir = %q, want %q", got, absCwd)
			}
			ctx := h.agentRuntimeContext("conv-1", ChatRequest{Message: "x"}, nil)
			if ctx["workspace_root"] != absCwd {
				t.Fatalf("workspace_root = %#v, want %q", ctx["workspace_root"], absCwd)
			}
		})
	}
}

func TestAgentRuntimeContextIncludesOpenAIConfig(t *testing.T) {
	h := &AgentHandler{
		config: &config.Config{
			OpenAI: config.OpenAIConfig{
				Provider: "openai",
				APIKey:   "test-key",
				BaseURL:  "https://api.example/v1",
				Model:    "test-model",
				Reasoning: config.OpenAIReasoningConfig{
					Effort: "low",
				},
			},
			AgentRuntime: config.AgentRuntimeConfig{
				MaxSteps:                     12,
				ToolTimeoutSeconds:           34,
				CompactionThresholdChars:     56,
				CompactionKeepRecentMessages: 7,
			},
		},
		configPath: filepath.Join("/tmp", "cyberstrike", "config.yaml"),
	}
	ctx := h.agentRuntimeContext("conv-1", ChatRequest{
		Role:                 "role-a",
		WebShellConnectionID: "ws-1",
		Reasoning:            &ChatReasoningRequest{Effort: "xhigh"},
	}, nil)
	if ctx["openai_api_key"] != "test-key" || ctx["openai_base_url"] != "https://api.example/v1" || ctx["openai_model"] != "test-model" {
		t.Fatalf("missing OpenAI config in context: %#v", ctx)
	}
	if ctx["openai_reasoning_effort"] != "xhigh" {
		t.Fatalf("openai_reasoning_effort = %#v, want xhigh", ctx["openai_reasoning_effort"])
	}
	if ctx["max_steps"] != 12 || ctx["tool_timeout_seconds"] != 34 {
		t.Fatalf("unexpected runtime limits in context: %#v", ctx)
	}
	if ctx["compaction_threshold_chars"] != 56 || ctx["compaction_keep_recent_messages"] != 7 {
		t.Fatalf("unexpected compaction limits in context: %#v", ctx)
	}
	wantStore := filepath.Join("/tmp", "cyberstrike", ".cyberstrike-agent-runtime", "sessions")
	if ctx["session_store_dir"] != wantStore {
		t.Fatalf("session_store_dir = %#v, want %q", ctx["session_store_dir"], wantStore)
	}
	if ctx["workspace_root"] != filepath.Join("/tmp", "cyberstrike") {
		t.Fatalf("workspace_root = %#v", ctx["workspace_root"])
	}
	if ctx["filesystem_enabled"] != true {
		t.Fatalf("filesystem_enabled = %#v, want true", ctx["filesystem_enabled"])
	}
}

func TestAgentRuntimeContextUsesConfiguredReasoningEffortWhenRequestOmitsIt(t *testing.T) {
	h := &AgentHandler{
		config: &config.Config{
			OpenAI: config.OpenAIConfig{
				Reasoning: config.OpenAIReasoningConfig{Effort: "low"},
			},
		},
	}
	ctx := h.agentRuntimeContext("conv-1", ChatRequest{Message: "x"}, nil)
	if ctx["openai_reasoning_effort"] != "low" {
		t.Fatalf("openai_reasoning_effort = %#v, want configured default", ctx["openai_reasoning_effort"])
	}
}

func TestAgentRuntimeContextDisablesFilesystemWhenEinoFilesystemDisabled(t *testing.T) {
	off := false
	h := &AgentHandler{
		config: &config.Config{
			AgentRuntime: config.AgentRuntimeConfig{Enabled: true},
			MultiAgent: config.MultiAgentConfig{
				EinoSkills: config.MultiAgentEinoSkillsConfig{
					FilesystemTools: &off,
				},
			},
		},
		configPath: filepath.Join("/tmp", "cyberstrike", "config.yaml"),
	}
	ctx := h.agentRuntimeContext("conv-1", ChatRequest{Message: "x"}, nil)
	if ctx["filesystem_enabled"] != false {
		t.Fatalf("filesystem_enabled = %#v, want false", ctx["filesystem_enabled"])
	}
}

func TestAgentRuntimeContextIncludesMCPEndpointAndAuth(t *testing.T) {
	h := &AgentHandler{
		config: &config.Config{
			MCP: config.MCPConfig{
				Enabled:         true,
				Host:            "0.0.0.0",
				Port:            8811,
				AuthHeader:      "X-MCP-Token",
				AuthHeaderValue: "secret-token",
			},
			AgentRuntime: config.AgentRuntimeConfig{
				MCPEnabled: true,
			},
		},
	}
	ctx := h.agentRuntimeContext("conv-1", ChatRequest{Message: "x"}, nil)
	if ctx["mcp_endpoint_url"] != "http://127.0.0.1:8811/mcp" {
		t.Fatalf("mcp_endpoint_url = %#v", ctx["mcp_endpoint_url"])
	}
	if ctx["mcp_auth_header"] != "X-MCP-Token" || ctx["mcp_auth_header_value"] != "secret-token" {
		t.Fatalf("missing MCP auth context: %#v", ctx)
	}
}

func TestAgentRuntimeContextLeavesMCPRegistryOwnedByRust(t *testing.T) {
	server := mcp.NewServer(zap.NewNop())
	server.RegisterTool(mcp.Tool{
		Name:             "read_file",
		Description:      "Read a file",
		ShortDescription: "Read file",
		InputSchema: map[string]interface{}{
			"type": "object",
			"properties": map[string]interface{}{
				"path": map[string]interface{}{"type": "string"},
			},
		},
	}, func(ctx context.Context, args map[string]interface{}) (*mcp.ToolResult, error) {
		return &mcp.ToolResult{Content: []mcp.Content{{Type: "text", Text: "ok"}}}, nil
	})
	h := &AgentHandler{
		config: &config.Config{
			MCP:          config.MCPConfig{Enabled: true, Host: "127.0.0.1", Port: 8811},
			AgentRuntime: config.AgentRuntimeConfig{MCPEnabled: true},
		},
		mcpServer: server,
	}

	ctx := h.agentRuntimeContext("conv-1", ChatRequest{Message: "x"}, nil)
	raw, ok := ctx["mcp_tools"].([]map[string]interface{})
	if !ok || len(raw) != 0 {
		t.Fatalf("mcp_tools = %#v, want empty Rust-owned registry payload", ctx["mcp_tools"])
	}
	if _, ok := ctx["mcp_tools_dir"].(string); !ok {
		t.Fatalf("mcp_tools_dir missing: %#v", ctx)
	}
}

func TestAgentRuntimeContextPassesRoleToolsForRustMCPFiltering(t *testing.T) {
	server := mcp.NewServer(zap.NewNop())
	for _, name := range []string{"read_file", "write_file"} {
		toolName := name
		server.RegisterTool(mcp.Tool{
			Name:        toolName,
			Description: toolName,
			InputSchema: map[string]interface{}{
				"type":       "object",
				"properties": map[string]interface{}{},
			},
		}, func(ctx context.Context, args map[string]interface{}) (*mcp.ToolResult, error) {
			return &mcp.ToolResult{Content: []mcp.Content{{Type: "text", Text: "ok"}}}, nil
		})
	}
	h := &AgentHandler{
		config: &config.Config{
			MCP:          config.MCPConfig{Enabled: true, Host: "127.0.0.1", Port: 8811},
			AgentRuntime: config.AgentRuntimeConfig{MCPEnabled: true},
		},
		mcpServer: server,
	}

	ctx := h.agentRuntimeContext("conv-1", ChatRequest{Message: "x"}, []string{"read_file"})
	raw, ok := ctx["mcp_tools"].([]map[string]interface{})
	if !ok || len(raw) != 0 {
		t.Fatalf("mcp_tools has unexpected type: %#v", ctx["mcp_tools"])
	}
	roleTools, ok := ctx["role_tools"].([]string)
	if !ok || len(roleTools) != 1 || roleTools[0] != "read_file" {
		t.Fatalf("role_tools = %#v, want read_file for Rust filtering", ctx["role_tools"])
	}
}

func TestAgentRuntimeContextPassesDefaultRoleToolsForRustMCPFiltering(t *testing.T) {
	server := mcp.NewServer(zap.NewNop())
	for _, name := range []string{"read_file", "write_file"} {
		toolName := name
		server.RegisterTool(mcp.Tool{
			Name:        toolName,
			Description: toolName,
			InputSchema: map[string]interface{}{
				"type":       "object",
				"properties": map[string]interface{}{},
			},
		}, func(ctx context.Context, args map[string]interface{}) (*mcp.ToolResult, error) {
			return &mcp.ToolResult{Content: []mcp.Content{{Type: "text", Text: "ok"}}}, nil
		})
	}
	h := &AgentHandler{
		config: &config.Config{
			MCP:          config.MCPConfig{Enabled: true, Host: "127.0.0.1", Port: 8811},
			AgentRuntime: config.AgentRuntimeConfig{MCPEnabled: true},
			Roles: map[string]config.RoleConfig{
				defaultRoleName: {
					Name:    defaultRoleName,
					Enabled: true,
					Tools:   []string{"read_file"},
				},
			},
		},
		mcpServer: server,
	}

	_, roleTools, roleName, ok := applyConfiguredRole(h.config, "", "x")
	if !ok || roleName != defaultRoleName {
		t.Fatalf("default role resolution ok=%v roleName=%q", ok, roleName)
	}
	ctx := h.agentRuntimeContext("conv-1", ChatRequest{Message: "x"}, roleTools)
	raw, ok := ctx["mcp_tools"].([]map[string]interface{})
	if !ok || len(raw) != 0 {
		t.Fatalf("mcp_tools has unexpected type: %#v", ctx["mcp_tools"])
	}
	ctxRoleTools, ok := ctx["role_tools"].([]string)
	if !ok || len(ctxRoleTools) != 1 || ctxRoleTools[0] != "read_file" {
		t.Fatalf("default role_tools = %#v, want read_file for Rust filtering", ctx["role_tools"])
	}
}

func TestAgentRuntimeMCPEndpointURLIPv6(t *testing.T) {
	h := &AgentHandler{
		config: &config.Config{
			MCP:          config.MCPConfig{Enabled: true, Host: "::1", Port: 8811},
			AgentRuntime: config.AgentRuntimeConfig{MCPEnabled: true},
		},
	}
	if got := h.agentRuntimeMCPEndpointURL(); got != "http://[::1]:8811/mcp" {
		t.Fatalf("agentRuntimeMCPEndpointURL = %q", got)
	}
}

func TestAgentRuntimeKnowledgeSnippetsFallbackReadsSQLiteWhenKnowledgeDisabled(t *testing.T) {
	root := t.TempDir()
	dbPath := filepath.Join(root, "knowledge.db")
	db, err := sql.Open("sqlite3", dbPath)
	if err != nil {
		t.Fatalf("open sqlite: %v", err)
	}
	defer db.Close()
	if _, err := db.Exec(`
CREATE TABLE knowledge_base_items (
	id TEXT PRIMARY KEY,
	category TEXT NOT NULL,
	title TEXT NOT NULL,
	file_path TEXT NOT NULL,
	content TEXT,
	created_at DATETIME NOT NULL,
	updated_at DATETIME NOT NULL
);
CREATE TABLE knowledge_embeddings (
	id TEXT PRIMARY KEY,
	item_id TEXT NOT NULL,
	chunk_index INTEGER NOT NULL,
	chunk_text TEXT NOT NULL,
	embedding TEXT NOT NULL,
	sub_indexes TEXT NOT NULL DEFAULT '',
	embedding_model TEXT NOT NULL DEFAULT '',
	embedding_dim INTEGER NOT NULL DEFAULT 0,
	created_at DATETIME NOT NULL
);
INSERT INTO knowledge_base_items (id, category, title, file_path, content, created_at, updated_at)
VALUES ('k1', 'web', 'Command Injection', 'cmd.md', 'OS command injection basics', datetime('now'), datetime('now'));
INSERT INTO knowledge_embeddings (id, item_id, chunk_index, chunk_text, embedding, created_at)
VALUES ('e1', 'k1', 0, 'command injection payloads and mitigations', '[]', datetime('now'));
`); err != nil {
		t.Fatalf("seed sqlite: %v", err)
	}
	h := &AgentHandler{
		config: &config.Config{
			Database:     config.DatabaseConfig{KnowledgeDBPath: dbPath},
			Knowledge:    config.KnowledgeConfig{Enabled: false},
			AgentRuntime: config.AgentRuntimeConfig{KnowledgeEnabled: true},
		},
		logger: zap.NewNop(),
	}

	snippets := h.agentRuntimeKnowledgeSnippets("conv-1", "command injection")

	if len(snippets) != 1 {
		t.Fatalf("snippets = %#v, want one sqlite fallback result", snippets)
	}
	if snippets[0]["id"] != "k1" || !strings.Contains(snippets[0]["content"].(string), "command injection") {
		t.Fatalf("unexpected snippet: %#v", snippets[0])
	}
}

func TestAgentRuntimeContextIncludesConversationHistory(t *testing.T) {
	h := &AgentHandler{config: &config.Config{}}
	ctx := h.agentRuntimeContext("conv-1", ChatRequest{Message: "current"}, nil, []agent.ChatMessage{
		{Role: "user", Content: "previous question"},
		{Role: "assistant", Content: "处理中..."},
		{Role: "assistant", Content: "previous answer", ReasoningContent: "thoughts"},
	})
	raw, ok := ctx["conversation_history"].([]map[string]string)
	if !ok {
		t.Fatalf("conversation_history has unexpected type: %#v", ctx["conversation_history"])
	}
	if len(raw) != 2 {
		t.Fatalf("conversation_history length = %d, want 2: %#v", len(raw), raw)
	}
	if raw[0]["role"] != "user" || raw[0]["content"] != "previous question" {
		t.Fatalf("unexpected first history item: %#v", raw[0])
	}
	if raw[1]["role"] != "assistant" || raw[1]["content"] != "previous answer" || raw[1]["reasoning_content"] != "thoughts" {
		t.Fatalf("unexpected second history item: %#v", raw[1])
	}
}

func TestAgentRuntimeContextIncludesApprovalAllowlist(t *testing.T) {
	h := &AgentHandler{
		config: &config.Config{
			AgentRuntime: config.AgentRuntimeConfig{ApprovalEnabled: true},
			Hitl:         config.HitlConfig{ToolWhitelist: []string{"knowledge_search", "mcp_call"}},
		},
	}
	ctx := h.agentRuntimeContext("conv-1", ChatRequest{
		Message: "x",
		Hitl: &HITLRequest{
			Enabled:        true,
			SensitiveTools: []string{"mcp_call", "custom_safe_tool"},
		},
	}, nil)
	if ctx["approval_enabled"] != true {
		t.Fatalf("approval_enabled = %#v, want true", ctx["approval_enabled"])
	}
	got, ok := ctx["approval_allowlist"].([]string)
	if !ok {
		t.Fatalf("approval_allowlist has unexpected type: %#v", ctx["approval_allowlist"])
	}
	want := []string{"knowledge_search", "mcp_call", "custom_safe_tool"}
	if strings.Join(got, ",") != strings.Join(want, ",") {
		t.Fatalf("approval_allowlist = %#v, want %#v", got, want)
	}
}

func TestHandleAgentRuntimeApprovalRequestedDoesNotComplete(t *testing.T) {
	h := &AgentHandler{}
	var finalResponse strings.Builder
	var reasoning strings.Builder
	var completed bool
	var aborted bool
	var pendingApproval bool
	var eventTypes []string

	err := h.handleAgentRuntimeEvent(func(eventType, message string, data interface{}) {
		eventTypes = append(eventTypes, eventType)
	}, "conv-1", "", agentruntime.Event{
		Type:             "approval_requested",
		ConversationID:   "conv-1",
		RuntimeSessionID: "session-1",
		TurnID:           "turn-1",
		RequestID:        "approval-call-1",
		Permission:       "mcp_call",
		Message:          "Tool mcp_call requires human approval before execution.",
	}, &finalResponse, &reasoning, &completed, &aborted, &pendingApproval)
	if err != nil {
		t.Fatalf("approval_requested handler: %v", err)
	}
	if completed || aborted {
		t.Fatalf("approval_requested should not complete or abort: completed=%v aborted=%v", completed, aborted)
	}
	if !pendingApproval {
		t.Fatalf("approval_requested should mark pending approval")
	}
	if len(eventTypes) != 1 || eventTypes[0] != "hitl_approval_requested" {
		t.Fatalf("unexpected event types: %#v", eventTypes)
	}
}

func TestAgentRuntimeContextPassesSkillsDirWithoutFullContentByDefault(t *testing.T) {
	root := t.TempDir()
	skillDir := filepath.Join(root, "demo")
	if err := os.MkdirAll(filepath.Join(skillDir, "references"), 0755); err != nil {
		t.Fatalf("mkdir skill: %v", err)
	}
	if err := os.WriteFile(filepath.Join(skillDir, "SKILL.md"), []byte("---\nname: demo\ndescription: Demo skill\n---\nUse references/guide.md when needed.\n"), 0644); err != nil {
		t.Fatalf("write SKILL.md: %v", err)
	}
	h := &AgentHandler{
		config:     &config.Config{AgentRuntime: config.AgentRuntimeConfig{SkillsEnabled: true}, SkillsDir: root},
		configPath: filepath.Join(root, "config.yaml"),
	}

	ctx := h.agentRuntimeContext("conv-1", ChatRequest{Message: "x"}, []string{"read_file", "skill:demo", "skill::other"})
	if ctx["skills_enabled"] != true {
		t.Fatalf("skills_enabled = %#v", ctx["skills_enabled"])
	}
	if ctx["skills_source"] != "rust_dir" {
		t.Fatalf("skills_source = %#v, want rust_dir", ctx["skills_source"])
	}
	if ctx["skills_dir"] != root {
		t.Fatalf("skills_dir = %#v, want %q", ctx["skills_dir"], root)
	}
	if _, ok := ctx["skills"]; ok {
		t.Fatalf("default runtime context should not include full skills content: %#v", ctx["skills"])
	}
	allowlist, ok := ctx["skills_allowlist"].([]string)
	if !ok {
		t.Fatalf("skills_allowlist type = %#v", ctx["skills_allowlist"])
	}
	if len(allowlist) != 2 || allowlist[0] != "demo" || allowlist[1] != "other" {
		t.Fatalf("skills_allowlist = %#v, want demo/other", allowlist)
	}
}

func TestAgentRuntimeSkillsSourceGoContextIncludesProgressivePackageData(t *testing.T) {
	root := t.TempDir()
	skillDir := filepath.Join(root, "demo")
	if err := os.MkdirAll(filepath.Join(skillDir, "references"), 0755); err != nil {
		t.Fatalf("mkdir skill: %v", err)
	}
	if err := os.WriteFile(filepath.Join(skillDir, "SKILL.md"), []byte("---\nname: demo\ndescription: Demo skill\n---\nUse references/guide.md when needed.\n"), 0644); err != nil {
		t.Fatalf("write SKILL.md: %v", err)
	}
	if err := os.WriteFile(filepath.Join(skillDir, "references", "guide.md"), []byte("Guide body"), 0644); err != nil {
		t.Fatalf("write reference: %v", err)
	}
	h := &AgentHandler{
		config:     &config.Config{AgentRuntime: config.AgentRuntimeConfig{SkillsEnabled: true, SkillsSource: "go_context"}, SkillsDir: root},
		configPath: filepath.Join(root, "config.yaml"),
	}

	ctx := h.agentRuntimeContext("conv-1", ChatRequest{Message: "x"}, nil)
	if ctx["skills_source"] != "go_context" {
		t.Fatalf("skills_source = %#v, want go_context", ctx["skills_source"])
	}
	skills, ok := ctx["skills"].(map[string]interface{})
	if !ok {
		t.Fatalf("skills missing or wrong type: %#v", ctx["skills"])
	}
	raw, ok := skills["demo"].(map[string]interface{})
	if !ok {
		t.Fatalf("skill package missing or wrong type: %#v", skills["demo"])
	}
	if !strings.Contains(raw["content"].(string), "references/guide.md") {
		t.Fatalf("skill content missing reference: %#v", raw["content"])
	}
	if _, ok := raw["resources"]; ok {
		t.Fatalf("skill resources should be lazy-loaded by runtime, got %#v", raw["resources"])
	}
	if raw["base_dir"] != skillDir {
		t.Fatalf("base_dir = %#v, want %q", raw["base_dir"], skillDir)
	}
	files, ok := raw["package_files"].([]map[string]interface{})
	if !ok || len(files) == 0 {
		t.Fatalf("package files missing: %#v", raw["package_files"])
	}
}

func TestAgentRuntimeTraceDataIncludesMCPIdentity(t *testing.T) {
	trace := agentRuntimeTraceData(agentruntime.Event{
		Type:             "tool_call_started",
		ConversationID:   "conv-1",
		RuntimeSessionID: "session-1",
		TurnID:           "turn-1",
		ToolCallID:       "call-1",
		ToolName:         "demo::lookup",
		Arguments:        map[string]interface{}{"query": "x"},
	})

	if trace["schema"] != "cyberstrike.agent_runtime.trace.v1" {
		t.Fatalf("unexpected schema: %#v", trace["schema"])
	}
	tool, ok := trace["tool"].(map[string]interface{})
	if !ok {
		t.Fatalf("tool trace missing: %#v", trace)
	}
	if tool["kind"] != "mcp" || tool["server"] != "demo" || tool["mcpName"] != "lookup" || tool["identity"] != "demo::lookup" {
		t.Fatalf("unexpected tool trace: %#v", tool)
	}
}

func TestAgentRuntimeTraceDataIncludesCompactionTask(t *testing.T) {
	trace := agentRuntimeTraceData(agentruntime.Event{
		Type:                    "compaction_completed",
		ConversationID:          "conv-1",
		RuntimeSessionID:        "session-1",
		TurnID:                  "turn-1",
		TaskID:                  "compaction_turn-1",
		Strategy:                "rollout_summary_with_recent_tail",
		InputMessageCount:       5,
		InputChars:              1234,
		ReplacementMessageCount: 3,
		ArtifactPath:            "/tmp/session/compactions/compaction_turn-1.json",
		Summary:                 "summary",
	})

	compaction, ok := trace["compaction"].(map[string]interface{})
	if !ok {
		t.Fatalf("compaction trace missing: %#v", trace)
	}
	if compaction["taskId"] != "compaction_turn-1" || compaction["strategy"] != "rollout_summary_with_recent_tail" || compaction["summary"] != "summary" || compaction["replacementMessageCount"] != 3 {
		t.Fatalf("unexpected compaction trace: %#v", compaction)
	}
	artifact, ok := compaction["artifact"].(map[string]interface{})
	if !ok || artifact["kind"] != "compaction_checkpoint" || artifact["path"] != "/tmp/session/compactions/compaction_turn-1.json" {
		t.Fatalf("unexpected compaction artifact trace: %#v", compaction["artifact"])
	}
}
