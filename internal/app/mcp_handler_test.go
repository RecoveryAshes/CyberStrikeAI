package app

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"cyberstrike-ai/internal/config"
	"cyberstrike-ai/internal/mcp"

	"go.uber.org/zap"
)

func TestMCPHandlerRoutesBuiltinNamespacedToolToInternalServer(t *testing.T) {
	server := mcp.NewServer(zap.NewNop())
	server.RegisterTool(mcp.Tool{
		Name:        "demo_builtin",
		Description: "demo builtin",
		InputSchema: map[string]interface{}{
			"type":       "object",
			"properties": map[string]interface{}{},
		},
	}, func(ctx context.Context, args map[string]interface{}) (*mcp.ToolResult, error) {
		return &mcp.ToolResult{Content: []mcp.Content{{Type: "text", Text: "builtin ok"}}}, nil
	})

	a := &App{
		config:         &config.Config{},
		mcpServer:      server,
		externalMCPMgr: mcp.NewExternalMCPManager(zap.NewNop()),
	}
	body := `{"jsonrpc":"2.0","id":"call-1","method":"tools/call","params":{"name":"builtin::demo_builtin","arguments":{}}}`
	req := httptest.NewRequest(http.MethodPost, "/mcp", strings.NewReader(body))
	rec := httptest.NewRecorder()

	a.mcpHandlerWithAuth(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", rec.Code, rec.Body.String())
	}
	var resp mcp.Message
	if err := json.Unmarshal(rec.Body.Bytes(), &resp); err != nil {
		t.Fatalf("decode response: %v", err)
	}
	if resp.Error != nil {
		t.Fatalf("unexpected JSON-RPC error: %#v", resp.Error)
	}
	if !strings.Contains(string(resp.Result), "builtin ok") {
		t.Fatalf("result = %s, want builtin ok", string(resp.Result))
	}
}
