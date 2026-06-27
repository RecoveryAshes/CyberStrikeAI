package handler

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/gin-gonic/gin"
	"go.uber.org/zap"
)

func TestOpenAPISpecIncludesAgentRuntimePathsAndBatchMode(t *testing.T) {
	gin.SetMode(gin.TestMode)
	router := gin.New()
	handler := NewOpenAPIHandler(nil, zap.NewNop(), nil, nil)
	router.GET("/openapi/spec", handler.GetOpenAPISpec)

	req := httptest.NewRequest(http.MethodGet, "/openapi/spec", nil)
	w := httptest.NewRecorder()
	router.ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}

	var spec map[string]interface{}
	if err := json.Unmarshal(w.Body.Bytes(), &spec); err != nil {
		t.Fatalf("unmarshal spec: %v", err)
	}
	paths, ok := spec["paths"].(map[string]interface{})
	if !ok {
		t.Fatalf("paths missing from spec")
	}
	if _, ok := paths["/api/agent-runtime/stream"]; !ok {
		t.Fatalf("/api/agent-runtime/stream missing from OpenAPI paths")
	}
	if _, ok := paths["/api/conversations/{id}/runtime-todos"]; !ok {
		t.Fatalf("/api/conversations/{id}/runtime-todos missing from OpenAPI paths")
	}

	components := spec["components"].(map[string]interface{})
	schemas := components["schemas"].(map[string]interface{})
	batchTaskRequest := schemas["BatchTaskRequest"].(map[string]interface{})
	properties := batchTaskRequest["properties"].(map[string]interface{})
	agentMode := properties["agentMode"].(map[string]interface{})
	enumValues, ok := agentMode["enum"].([]interface{})
	if !ok {
		t.Fatalf("agentMode enum missing: %#v", agentMode["enum"])
	}
	found := false
	for _, value := range enumValues {
		if value == "agent_runtime" {
			found = true
			break
		}
	}
	if !found {
		t.Fatalf("agent_runtime missing from BatchTaskRequest agentMode enum: %#v", enumValues)
	}
}
