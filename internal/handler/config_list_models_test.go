package handler

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"cyberstrike-ai/internal/config"

	"github.com/gin-gonic/gin"
	"go.uber.org/zap"
)

func TestListModelsUsesCurrentOpenAIConfigWhenBodyEmpty(t *testing.T) {
	gin.SetMode(gin.TestMode)
	var gotAuth string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/models" {
			t.Fatalf("unexpected path: %s", r.URL.Path)
		}
		gotAuth = r.Header.Get("Authorization")
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"object":"list","data":[{"id":"z-model"},{"id":"a-model"}]}`))
	}))
	defer upstream.Close()

	router := gin.New()
	h := NewConfigHandler("", &config.Config{
		OpenAI: config.OpenAIConfig{
			Provider: "openai",
			BaseURL:  upstream.URL + "/v1",
			APIKey:   "current-key",
		},
	}, nil, nil, nil, nil, nil, zap.NewNop())
	router.POST("/api/config/list-models", h.ListModels)

	req := httptest.NewRequest(http.MethodPost, "/api/config/list-models", nil)
	w := httptest.NewRecorder()
	router.ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}
	if gotAuth != "Bearer current-key" {
		t.Fatalf("Authorization = %q, want Bearer current-key", gotAuth)
	}
	var body struct {
		Success bool     `json:"success"`
		Models  []string `json:"models"`
		Count   int      `json:"count"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &body); err != nil {
		t.Fatalf("unmarshal response: %v", err)
	}
	if !body.Success {
		t.Fatalf("success = false, body = %s", w.Body.String())
	}
	if body.Count != 2 || strings.Join(body.Models, ",") != "a-model,z-model" {
		t.Fatalf("models = %#v, count = %d", body.Models, body.Count)
	}
}

func TestListModelsAllowsCamelCaseOverride(t *testing.T) {
	gin.SetMode(gin.TestMode)
	var gotAuth string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotAuth = r.Header.Get("Authorization")
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"data":[{"id":"override-model"}]}`))
	}))
	defer upstream.Close()

	router := gin.New()
	h := NewConfigHandler("", &config.Config{
		OpenAI: config.OpenAIConfig{
			Provider: "openai",
			BaseURL:  "https://unused.example/v1",
			APIKey:   "current-key",
		},
	}, nil, nil, nil, nil, nil, zap.NewNop())
	router.POST("/api/config/list-models", h.ListModels)

	req := httptest.NewRequest(
		http.MethodPost,
		"/api/config/list-models",
		strings.NewReader(`{"baseURL":"`+upstream.URL+`","apiKey":"override-key"}`),
	)
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	router.ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}
	if gotAuth != "Bearer override-key" {
		t.Fatalf("Authorization = %q, want Bearer override-key", gotAuth)
	}
	var body struct {
		Success bool     `json:"success"`
		Models  []string `json:"models"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &body); err != nil {
		t.Fatalf("unmarshal response: %v", err)
	}
	if !body.Success || len(body.Models) != 1 || body.Models[0] != "override-model" {
		t.Fatalf("unexpected response: %s", w.Body.String())
	}
}
