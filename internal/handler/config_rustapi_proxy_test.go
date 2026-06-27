package handler

import (
	"bytes"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"

	"cyberstrike-ai/internal/config"

	"github.com/gin-gonic/gin"
	"go.uber.org/zap"
)

type seenConfigProxyRequest struct {
	Method string
	Path   string
	Query  string
	Body   string
}

type recordingAgentUpdater struct {
	openAIConfig config.OpenAIConfig
	updateCalls  int
}

func (r *recordingAgentUpdater) UpdateConfig(cfg *config.OpenAIConfig) {
	if cfg != nil {
		r.openAIConfig = *cfg
	}
	r.updateCalls++
}

func (r *recordingAgentUpdater) UpdateMaxIterations(maxIterations int) {}

func (r *recordingAgentUpdater) UpdateToolDescriptionMode(mode string) {}

func TestConfigEndpointsProxyToRustAPI(t *testing.T) {
	gin.SetMode(gin.TestMode)
	var seen []seenConfigProxyRequest
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		seen = append(seen, seenConfigProxyRequest{
			Method: r.Method,
			Path:   r.URL.Path,
			Query:  r.URL.RawQuery,
			Body:   string(body),
		})
		w.Header().Set("Content-Type", "application/json")
		switch {
		case r.Method == http.MethodGet && r.URL.Path == "/api/config":
			_, _ = w.Write([]byte(`{"openai":{"provider":"openai","api_key":"pg-key","base_url":"http://pg/v1","model":"pg-model","reasoning":{"effort":"xhigh"}}}`))
		case r.Method == http.MethodPut && r.URL.Path == "/api/config":
			_, _ = w.Write([]byte(`{"openai":{"provider":"openai","api_key":"k","base_url":"http://base/v1","model":"rust-model","reasoning":{"effort":"low"}}}`))
		case r.Method == http.MethodPost && r.URL.Path == "/api/config/list-models":
			_, _ = w.Write([]byte(`{"success":true,"supported":true,"models":["rust-model"],"count":1}`))
		default:
			http.NotFound(w, r)
		}
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	router := gin.New()
	cfg := &config.Config{
		OpenAI: config.OpenAIConfig{
			Provider:       "openai",
			APIKey:         "boot-key",
			BaseURL:        "http://boot/v1",
			Model:          "boot-model",
			MaxTotalTokens: 120000,
			Reasoning: config.OpenAIReasoningConfig{
				Effort:  "xhigh",
				Profile: "openai_compat",
			},
		},
		AgentRuntime: config.AgentRuntimeConfig{
			Enabled:            true,
			Transport:          "grpc",
			BinaryPath:         "/opt/runtime",
			ToolTimeoutSeconds: 180,
		},
	}
	h := NewConfigHandler("", cfg, nil, nil, nil, nil, nil, zap.NewNop())
	router.GET("/api/config", h.GetConfig)
	router.PUT("/api/config", h.UpdateConfig)
	router.POST("/api/config/list-models", h.ListModels)

	cases := []struct {
		method string
		path   string
		body   string
	}{
		{http.MethodGet, "/api/config", ""},
		{http.MethodPut, "/api/config", `{"openai":{"model":"rust-model"}}`},
		{http.MethodPost, "/api/config/list-models", ""},
	}

	for _, tc := range cases {
		var body io.Reader
		if tc.body != "" {
			body = bytes.NewBufferString(tc.body)
		}
		req := httptest.NewRequest(tc.method, tc.path, body)
		if tc.body != "" {
			req.Header.Set("Content-Type", "application/json")
		}
		w := httptest.NewRecorder()
		router.ServeHTTP(w, req)
		if w.Code != http.StatusOK {
			t.Fatalf("%s %s status = %d, body = %s", tc.method, tc.path, w.Code, w.Body.String())
		}
	}

	if len(seen) != 3 {
		t.Fatalf("seen len = %d, want 3: %#v", len(seen), seen)
	}
	for i, req := range seen {
		if req.Query != "" {
			t.Fatalf("seen[%d] unexpectedly forwarded query: %#v", i, req)
		}
	}
	want := []seenConfigProxyRequest{
		{Method: http.MethodGet, Path: "/api/config"},
		{Method: http.MethodPut, Path: "/api/config", Body: `{"openai":{"model":"rust-model"}}`},
		{Method: http.MethodPost, Path: "/api/config/list-models"},
	}
	for i := range want {
		if seen[i].Method != want[i].Method || seen[i].Path != want[i].Path || seen[i].Query != want[i].Query || (want[i].Body != "" && seen[i].Body != want[i].Body) {
			t.Fatalf("seen[%d] = %#v, want %#v", i, seen[i], want[i])
		}
	}
	if cfg.OpenAI.Provider != "openai" || cfg.OpenAI.APIKey != "k" || cfg.OpenAI.BaseURL != "http://base/v1" || cfg.OpenAI.Model != "rust-model" {
		t.Fatalf("cfg.OpenAI = %#v, want PostgreSQL frontend OpenAI config from Rust response", cfg.OpenAI)
	}
	if cfg.OpenAI.MaxTotalTokens != 120000 || cfg.OpenAI.Reasoning.Profile != "openai_compat" || cfg.OpenAI.Reasoning.Effort != "low" {
		t.Fatalf("runtime-only OpenAI fields changed unexpectedly: %#v", cfg.OpenAI)
	}
	if !cfg.AgentRuntime.Enabled || cfg.AgentRuntime.Transport != "grpc" || cfg.AgentRuntime.BinaryPath != "/opt/runtime" || cfg.AgentRuntime.ToolTimeoutSeconds != 180 {
		t.Fatalf("agent runtime fields changed unexpectedly: %#v", cfg.AgentRuntime)
	}
}

func TestConfigPutRefreshesAgentRuntimeOpenAIConfigFromRustResponse(t *testing.T) {
	gin.SetMode(gin.TestMode)
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPut || r.URL.Path != "/api/config" {
			http.NotFound(w, r)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"openai":{"provider":"pg","api_key":"pg-key","base_url":"http://pg/v1","model":"pg-model","reasoning":{"effort":"low"}}}`))
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	router := gin.New()
	cfg := &config.Config{
		OpenAI: config.OpenAIConfig{
			Provider:       "local",
			APIKey:         "local-key",
			BaseURL:        "http://local/v1",
			Model:          "local-model",
			MaxTotalTokens: 120000,
			Reasoning: config.OpenAIReasoningConfig{
				Mode:    "auto",
				Effort:  "xhigh",
				Profile: "openai_compat",
			},
		},
	}
	agentUpdater := &recordingAgentUpdater{}
	h := NewConfigHandler("", cfg, nil, nil, agentUpdater, nil, nil, zap.NewNop())
	router.PUT("/api/config", h.UpdateConfig)

	req := httptest.NewRequest(http.MethodPut, "/api/config", bytes.NewBufferString(`{"openai":{"model":"pg-model"}}`))
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	router.ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}

	if agentUpdater.updateCalls != 1 {
		t.Fatalf("agent update calls = %d, want 1", agentUpdater.updateCalls)
	}
	if agentUpdater.openAIConfig.Provider != "pg" || agentUpdater.openAIConfig.APIKey != "pg-key" || agentUpdater.openAIConfig.BaseURL != "http://pg/v1" || agentUpdater.openAIConfig.Model != "pg-model" {
		t.Fatalf("agent OpenAI config = %#v, want Rust PostgreSQL response", agentUpdater.openAIConfig)
	}
	if agentUpdater.openAIConfig.MaxTotalTokens != 120000 || agentUpdater.openAIConfig.Reasoning.Mode != "auto" || agentUpdater.openAIConfig.Reasoning.Profile != "openai_compat" || agentUpdater.openAIConfig.Reasoning.Effort != "low" {
		t.Fatalf("agent runtime-only OpenAI fields changed unexpectedly: %#v", agentUpdater.openAIConfig)
	}
}

func TestConfigPutOnlyForwardsFrontendOpenAIFieldsToRustAPI(t *testing.T) {
	gin.SetMode(gin.TestMode)
	var seen seenConfigProxyRequest
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		seen = seenConfigProxyRequest{
			Method: r.Method,
			Path:   r.URL.Path,
			Query:  r.URL.RawQuery,
			Body:   string(body),
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"openai":{"provider":"pg","api_key":"pg-key","base_url":"http://pg/v1","model":"pg-model","reasoning":{"effort":"low"}},"agent_runtime":{"enabled":false},"vision":{"enabled":false}}`))
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	configPath := filepath.Join(t.TempDir(), "config.yaml")
	originalConfig := []byte("openai:\n  model: file-model\n")
	if err := os.WriteFile(configPath, originalConfig, 0600); err != nil {
		t.Fatalf("write config fixture: %v", err)
	}

	router := gin.New()
	cfg := &config.Config{
		OpenAI: config.OpenAIConfig{
			Provider:       "local",
			APIKey:         "local-key",
			BaseURL:        "http://local/v1",
			Model:          "local-model",
			MaxTotalTokens: 120000,
			Reasoning: config.OpenAIReasoningConfig{
				Mode:    "auto",
				Effort:  "xhigh",
				Profile: "openai_compat",
			},
		},
		Vision: config.VisionConfig{
			Enabled: true,
			Model:   "vision-model",
		},
		AgentRuntime: config.AgentRuntimeConfig{
			Enabled:            true,
			Transport:          "grpc",
			BinaryPath:         "/opt/runtime",
			ToolTimeoutSeconds: 180,
		},
	}
	h := NewConfigHandler(configPath, cfg, nil, nil, nil, nil, nil, zap.NewNop())
	router.PUT("/api/config", h.UpdateConfig)

	body := `{"openai":{"model":"pg-model"},"agent_runtime":{"enabled":false},"vision":{"enabled":false}}`
	req := httptest.NewRequest(http.MethodPut, "/api/config", bytes.NewBufferString(body))
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	router.ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}

	if seen.Method != http.MethodPut || seen.Path != "/api/config" || seen.Body != `{"openai":{"model":"pg-model"}}` {
		t.Fatalf("seen = %#v, want PUT /api/config with only frontend OpenAI config fields", seen)
	}
	if cfg.OpenAI.Provider != "pg" || cfg.OpenAI.APIKey != "pg-key" || cfg.OpenAI.BaseURL != "http://pg/v1" || cfg.OpenAI.Model != "pg-model" {
		t.Fatalf("Go facade OpenAI config was not refreshed from PostgreSQL response: %#v", cfg.OpenAI)
	}
	if cfg.OpenAI.MaxTotalTokens != 120000 || cfg.OpenAI.Reasoning.Mode != "auto" || cfg.OpenAI.Reasoning.Profile != "openai_compat" || cfg.OpenAI.Reasoning.Effort != "low" {
		t.Fatalf("runtime-only OpenAI fields changed unexpectedly: %#v", cfg.OpenAI)
	}
	if !cfg.Vision.Enabled || cfg.Vision.Model != "vision-model" {
		t.Fatalf("vision config was changed by frontend config proxy: %#v", cfg.Vision)
	}
	if !cfg.AgentRuntime.Enabled || cfg.AgentRuntime.Transport != "grpc" || cfg.AgentRuntime.BinaryPath != "/opt/runtime" || cfg.AgentRuntime.ToolTimeoutSeconds != 180 {
		t.Fatalf("agent runtime config was changed by frontend config proxy: %#v", cfg.AgentRuntime)
	}
	after, err := os.ReadFile(configPath)
	if err != nil {
		t.Fatalf("read config fixture: %v", err)
	}
	if string(after) != string(originalConfig) {
		t.Fatalf("PUT /api/config wrote config file:\n got: %q\nwant: %q", after, originalConfig)
	}
}

func TestConfigPutDoesNotSeedFromGoRuntimeConfig(t *testing.T) {
	gin.SetMode(gin.TestMode)
	var seen []seenConfigProxyRequest
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		seen = append(seen, seenConfigProxyRequest{
			Method: r.Method,
			Path:   r.URL.Path,
			Body:   string(body),
		})
		w.Header().Set("Content-Type", "application/json")
		switch {
		case r.Method == http.MethodPut && r.URL.Path == "/api/config":
			_, _ = w.Write([]byte(`{"openai":{"provider":"pg","api_key":"pg-key","base_url":"http://pg/v1","model":"pg-model","reasoning":{"effort":"low"}}}`))
		default:
			http.NotFound(w, r)
		}
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	router := gin.New()
	cfg := &config.Config{
		OpenAI: config.OpenAIConfig{
			Provider:       "openai",
			APIKey:         "boot-key",
			BaseURL:        "http://boot/v1",
			Model:          "boot-model",
			MaxTotalTokens: 120000,
			Reasoning: config.OpenAIReasoningConfig{
				Effort:  "xhigh",
				Profile: "openai_compat",
			},
		},
		AgentRuntime: config.AgentRuntimeConfig{
			Enabled: true,
		},
	}
	h := NewConfigHandler("/must/not/be/read.yaml", cfg, nil, nil, nil, nil, nil, zap.NewNop())
	router.PUT("/api/config", h.UpdateConfig)

	req := httptest.NewRequest(http.MethodPut, "/api/config", bytes.NewBufferString(`{"openai":{"model":"pg-model"},"vision":{"enabled":false}}`))
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	router.ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}

	want := []seenConfigProxyRequest{
		{Method: http.MethodPut, Path: "/api/config", Body: `{"openai":{"model":"pg-model"}}`},
	}
	if len(seen) != len(want) {
		t.Fatalf("seen = %#v, want %#v", seen, want)
	}
	for i := range want {
		if seen[i] != want[i] {
			t.Fatalf("seen[%d] = %#v, want %#v", i, seen[i], want[i])
		}
	}
}

func TestProjectFrontendConfigUpdateBodyKeepsOnlyChatWebOpenAIFields(t *testing.T) {
	raw := []byte(`{
		"openai": {
			"provider": "openai",
			"api_key": "key",
			"base_url": "http://base/v1",
			"model": "model",
			"max_total_tokens": 123,
			"reasoning": {
				"effort": "low",
				"profile": "openai_compat"
			}
		},
		"vision": {"enabled": false},
		"agent_runtime": {"enabled": false},
		"tools": [{"name": "nmap", "enabled": false}]
	}`)
	projected, err := projectFrontendConfigUpdateBody(raw)
	if err != nil {
		t.Fatalf("projectFrontendConfigUpdateBody error: %v", err)
	}
	want := `{"openai":{"api_key":"key","base_url":"http://base/v1","model":"model","provider":"openai","reasoning":{"effort":"low"}}}`
	if string(projected) != want {
		t.Fatalf("projected = %s, want %s", projected, want)
	}
}

func TestConfigGetProxiesRustAPIStatus(t *testing.T) {
	gin.SetMode(gin.TestMode)
	getAttempts := 0
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method == http.MethodGet && r.URL.Path == "/api/config" {
			getAttempts++
			if getAttempts == 1 {
				http.Error(w, "not ready", http.StatusServiceUnavailable)
				return
			}
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"openai":{"model":"pg-model"}}`))
			return
		}
		http.NotFound(w, r)
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	router := gin.New()
	h := NewConfigHandler("", &config.Config{
		OpenAI: config.OpenAIConfig{Model: "boot-model"},
	}, nil, nil, nil, nil, nil, zap.NewNop())
	router.GET("/api/config", h.GetConfig)

	req := httptest.NewRequest(http.MethodGet, "/api/config", nil)
	first := httptest.NewRecorder()
	router.ServeHTTP(first, req)
	if first.Code != http.StatusServiceUnavailable {
		t.Fatalf("first status = %d, want 503", first.Code)
	}
	second := httptest.NewRecorder()
	router.ServeHTTP(second, httptest.NewRequest(http.MethodGet, "/api/config", nil))
	if second.Code != http.StatusOK {
		t.Fatalf("second status = %d, body = %s", second.Code, second.Body.String())
	}
	if getAttempts != 2 {
		t.Fatalf("get attempts = %d, want 2", getAttempts)
	}
	if second.Body.String() != `{"openai":{"model":"pg-model"}}` {
		t.Fatalf("unexpected second body: %s", second.Body.String())
	}
}
