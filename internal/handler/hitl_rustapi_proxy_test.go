package handler

import (
	"bytes"
	"context"
	"io"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"cyberstrike-ai/internal/config"
	"cyberstrike-ai/internal/database"

	"github.com/gin-gonic/gin"
	"go.uber.org/zap"
)

type recordingHitlWhitelistSaver struct {
	calls int
}

func (r *recordingHitlWhitelistSaver) MergeHitlToolWhitelistIntoConfig(add []string) error {
	r.calls++
	return nil
}

func TestHITLConfigEndpointsProxyToRustAPI(t *testing.T) {
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
		case r.Method == http.MethodGet && r.URL.Path == "/api/hitl/config/conv-1":
			_, _ = w.Write([]byte(`{"conversationId":"conv-1","hitl":{"enabled":true,"mode":"approval","sensitiveTools":["nmap"],"timeoutSeconds":600},"hitlGlobalToolWhitelist":[]}`))
		case r.Method == http.MethodPut && r.URL.Path == "/api/hitl/config":
			_, _ = w.Write([]byte(`{"ok":true}`))
		default:
			http.NotFound(w, r)
		}
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	saver := &recordingHitlWhitelistSaver{}
	h := &AgentHandler{
		logger:             zap.NewNop(),
		config:             &config.Config{},
		hitlManager:        NewHITLManager(nil, zap.NewNop()),
		hitlWhitelistSaver: saver,
		httpClient:         http.DefaultClient,
	}

	router := gin.New()
	router.GET("/api/hitl/config/:conversationId", h.GetHITLConversationConfig)
	router.PUT("/api/hitl/config", h.UpsertHITLConversationConfig)

	getReq := httptest.NewRequest(http.MethodGet, "/api/hitl/config/conv-1", nil)
	getW := httptest.NewRecorder()
	router.ServeHTTP(getW, getReq)
	if getW.Code != http.StatusOK {
		t.Fatalf("GET status = %d, body = %s", getW.Code, getW.Body.String())
	}

	body := `{"conversationId":"conv-1","enabled":true,"mode":"approval","sensitiveTools":["nmap"],"timeoutSeconds":600}`
	putReq := httptest.NewRequest(http.MethodPut, "/api/hitl/config", bytes.NewBufferString(body))
	putReq.Header.Set("Content-Type", "application/json")
	putW := httptest.NewRecorder()
	router.ServeHTTP(putW, putReq)
	if putW.Code != http.StatusOK {
		t.Fatalf("PUT status = %d, body = %s", putW.Code, putW.Body.String())
	}
	if saver.calls != 0 {
		t.Fatalf("unexpected config.yaml whitelist saver calls = %d", saver.calls)
	}
	if len(seen) != 2 {
		t.Fatalf("seen len = %d, want 2: %#v", len(seen), seen)
	}
	if seen[0].Method != http.MethodGet || seen[0].Path != "/api/hitl/config/conv-1" {
		t.Fatalf("seen[0] = %#v", seen[0])
	}
	if seen[1].Method != http.MethodPut || seen[1].Path != "/api/hitl/config" || seen[1].Body != body {
		t.Fatalf("seen[1] = %#v", seen[1])
	}
}

func TestHITLPendingAndDecisionProxyToRustAPI(t *testing.T) {
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
		case r.Method == http.MethodGet && r.URL.Path == "/api/hitl/pending":
			_, _ = w.Write([]byte(`{"items":[],"page":1,"pageSize":50}`))
		case r.Method == http.MethodPost && r.URL.Path == "/api/hitl/decision":
			_, _ = w.Write([]byte(`{"ok":true,"resumed":true}`))
		default:
			http.NotFound(w, r)
		}
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	pending := &pendingInterrupt{
		InterruptID: "hitl_local",
		Mode:        "approval",
		decideCh:    make(chan hitlDecision, 1),
	}
	manager := &HITLManager{
		logger:  zap.NewNop(),
		runtime: make(map[string]hitlRuntimeConfig),
		pending: map[string]*pendingInterrupt{pending.InterruptID: pending},
	}
	manager.SetMirror(&failingHitlMirror{t: t})
	h := &AgentHandler{
		logger:      zap.NewNop(),
		config:      &config.Config{},
		hitlManager: manager,
		httpClient:  http.DefaultClient,
	}

	router := gin.New()
	router.GET("/api/hitl/pending", h.ListHITLPending)
	router.POST("/api/hitl/decision", h.DecideHITLInterrupt)

	pendingReq := httptest.NewRequest(http.MethodGet, "/api/hitl/pending?status=pending&pageSize=50&conversationId=conv-1", nil)
	pendingW := httptest.NewRecorder()
	router.ServeHTTP(pendingW, pendingReq)
	if pendingW.Code != http.StatusOK {
		t.Fatalf("pending status = %d, body = %s", pendingW.Code, pendingW.Body.String())
	}

	decisionBody := `{"interruptId":"hitl_local","decision":"approve","comment":"ok"}`
	decisionReq := httptest.NewRequest(http.MethodPost, "/api/hitl/decision", bytes.NewBufferString(decisionBody))
	decisionReq.Header.Set("Content-Type", "application/json")
	decisionW := httptest.NewRecorder()
	router.ServeHTTP(decisionW, decisionReq)
	if decisionW.Code != http.StatusOK {
		t.Fatalf("decision status = %d, body = %s", decisionW.Code, decisionW.Body.String())
	}
	if decisionW.Body.String() != `{"ok":true,"resumed":true}` {
		t.Fatalf("decision body = %s", decisionW.Body.String())
	}
	select {
	case got := <-pending.decideCh:
		t.Fatalf("Go HITL pending should not be woken by Rust-owned decision, got %#v", got)
	case <-time.After(50 * time.Millisecond):
	}
	if len(seen) != 2 {
		t.Fatalf("seen len = %d, want 2: %#v", len(seen), seen)
	}
	if seen[0].Method != http.MethodGet || seen[0].Path != "/api/hitl/pending" || seen[0].Query != "status=pending&pageSize=50&conversationId=conv-1" {
		t.Fatalf("seen[0] = %#v", seen[0])
	}
	if seen[1].Method != http.MethodPost || seen[1].Path != "/api/hitl/decision" || seen[1].Body != decisionBody {
		t.Fatalf("seen[1] = %#v", seen[1])
	}
}

func TestHITLManagerMirrorsPendingInterruptToRustAPI(t *testing.T) {
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
		if r.Method != http.MethodPost || r.URL.Path != "/api/internal/hitl/interrupts" {
			http.NotFound(w, r)
			return
		}
		_, _ = w.Write([]byte(`{"ok":true}`))
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	db, err := database.NewDB(t.TempDir()+"/hitl.sqlite", zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	defer db.Close()
	manager := NewHITLManager(db, zap.NewNop())
	if err := manager.EnsureSchema(); err != nil {
		t.Fatalf("EnsureSchema: %v", err)
	}
	manager.SetMirror(&rustHITLInterruptMirror{client: upstream.Client(), logger: zap.NewNop()})

	p, err := manager.CreatePendingInterruptWithID("hitl_pg", "conv-1", "msg-1", "approval", "nmap", "tool-1", `{"cmd":"nmap"}`)
	if err != nil {
		t.Fatalf("CreatePendingInterruptWithID: %v", err)
	}
	if p.InterruptID != "hitl_pg" {
		t.Fatalf("pending = %#v", p)
	}
	wantBody := `{"conversationId":"conv-1","id":"hitl_pg","messageId":"msg-1","mode":"approval","payload":"{\"cmd\":\"nmap\"}","status":"pending","toolCallId":"tool-1","toolName":"nmap"}`
	if seen.Method != http.MethodPost || seen.Path != "/api/internal/hitl/interrupts" || seen.Body != wantBody {
		t.Fatalf("seen = %#v, want body %s", seen, wantBody)
	}
}

type failingHitlMirror struct {
	t *testing.T
}

func (f *failingHitlMirror) CreatePendingInterrupt(context.Context, hitlInterruptRecord) error {
	f.t.Fatalf("CreatePendingInterrupt mirror should not be called")
	return nil
}

func (f *failingHitlMirror) ResolveInterrupt(context.Context, string, string, string, map[string]interface{}) error {
	f.t.Fatalf("ResolveInterrupt mirror should not be called after Rust decision proxy succeeded")
	return nil
}
