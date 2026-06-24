package handler

import (
	"context"
	"database/sql"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"cyberstrike-ai/internal/agent"
	"cyberstrike-ai/internal/agentruntime"
	"cyberstrike-ai/internal/config"
	"cyberstrike-ai/internal/database"
	"cyberstrike-ai/internal/mcp"

	_ "github.com/mattn/go-sqlite3"
	"go.uber.org/zap"
)

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

func TestAgentRuntimeContextIncludesBuiltinMCPTools(t *testing.T) {
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
	if !ok || len(raw) != 1 {
		t.Fatalf("mcp_tools = %#v, want one builtin tool", ctx["mcp_tools"])
	}
	tool := raw[0]
	if tool["server"] != "builtin" || tool["name"] != "read_file" || tool["call_name"] != "read_file" || tool["model_name"] != "read_file" {
		t.Fatalf("unexpected builtin MCP tool: %#v", tool)
	}
	if tool["transport"] != "builtin" {
		t.Fatalf("transport = %#v, want builtin", tool["transport"])
	}
	if tool["description"] != "Read file" {
		t.Fatalf("description = %#v", tool["description"])
	}
}

func TestAgentRuntimeContextFiltersBuiltinMCPToolsByRole(t *testing.T) {
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
	if !ok {
		t.Fatalf("mcp_tools has unexpected type: %#v", ctx["mcp_tools"])
	}
	if len(raw) != 1 || raw[0]["name"] != "read_file" {
		t.Fatalf("filtered mcp_tools = %#v, want read_file only", raw)
	}
}

func TestAgentRuntimeContextFiltersBuiltinMCPToolsByDefaultRole(t *testing.T) {
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
	if !ok {
		t.Fatalf("mcp_tools has unexpected type: %#v", ctx["mcp_tools"])
	}
	if len(raw) != 1 || raw[0]["name"] != "read_file" {
		t.Fatalf("default role filtered mcp_tools = %#v, want read_file only", raw)
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

func TestAgentRuntimeSkillsIncludeProgressivePackageData(t *testing.T) {
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
		config:     &config.Config{AgentRuntime: config.AgentRuntimeConfig{SkillsEnabled: true}, SkillsDir: root},
		configPath: filepath.Join(root, "config.yaml"),
	}

	skills := h.agentRuntimeSkills()
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
