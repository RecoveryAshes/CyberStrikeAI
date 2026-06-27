package handler

import (
	"bytes"
	"io"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"

	"cyberstrike-ai/internal/database"

	"github.com/gin-gonic/gin"
	"go.uber.org/zap"
)

func newConversationProxyTestDB(t *testing.T) (*database.DB, *database.Conversation, *database.Message) {
	t.Helper()
	db, err := database.NewDB(filepath.Join(t.TempDir(), "conversations.sqlite"), zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	conv, err := db.CreateConversationWithID("conv-local", "", "Local Conversation", database.ConversationCreateMeta{})
	if err != nil {
		t.Fatalf("CreateConversationWithID: %v", err)
	}
	msg, err := db.AddMessage(conv.ID, "user", "hello pg", nil)
	if err != nil {
		t.Fatalf("AddMessage: %v", err)
	}
	if err := db.AddProcessDetail(msg.ID, conv.ID, "progress", "started", map[string]interface{}{"step": "one"}); err != nil {
		t.Fatalf("AddProcessDetail: %v", err)
	}
	return db, conv, msg
}

func TestConversationListProxiesToRustAPIWithoutReadBackfill(t *testing.T) {
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
		case r.Method == http.MethodGet && r.URL.Path == "/api/conversations":
			_, _ = w.Write([]byte(`{"conversations":[{"id":"conv-local","title":"PG Conversation","pinned":false,"createdAt":"2026-06-26T00:00:00Z","updatedAt":"2026-06-26T00:00:00Z"}],"total":1,"limit":200,"offset":0}`))
		default:
			http.NotFound(w, r)
		}
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	db, _, _ := newConversationProxyTestDB(t)
	defer db.Close()

	router := gin.New()
	h := NewConversationHandler(db, zap.NewNop())
	router.GET("/api/conversations", h.ListConversations)

	w := httptest.NewRecorder()
	router.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/api/conversations?limit=200&sort_by=updated_at", nil))
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}
	if len(seen) != 1 {
		t.Fatalf("seen len = %d, want only proxied GET: %#v", len(seen), seen)
	}
	if seen[0].Method != http.MethodGet || seen[0].Path != "/api/conversations" || seen[0].Query != "limit=200&sort_by=updated_at" {
		t.Fatalf("proxied GET = %#v", seen[0])
	}
	if w.Body.String() != `{"conversations":[{"id":"conv-local","title":"PG Conversation","pinned":false,"createdAt":"2026-06-26T00:00:00Z","updatedAt":"2026-06-26T00:00:00Z"}],"total":1,"limit":200,"offset":0}` {
		t.Fatalf("unexpected body: %s", w.Body.String())
	}
}

func TestConversationListDoesNotBackfillFromSQLiteOnRead(t *testing.T) {
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
		case r.Method == http.MethodGet && r.URL.Path == "/api/conversations":
			_, _ = w.Write([]byte(`{"conversations":[],"total":0,"limit":200,"offset":0}`))
		default:
			http.NotFound(w, r)
		}
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	db, _, _ := newConversationProxyTestDB(t)
	defer db.Close()

	router := gin.New()
	h := NewConversationHandler(db, zap.NewNop())
	router.GET("/api/conversations", h.ListConversations)

	w := httptest.NewRecorder()
	router.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/api/conversations?limit=200", nil))
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}
	if len(seen) != 1 {
		t.Fatalf("seen len = %d, want only proxied GET: %#v", len(seen), seen)
	}
	if seen[0].Method != http.MethodGet || seen[0].Path != "/api/conversations" || seen[0].Query != "limit=200" {
		t.Fatalf("seen[0] = %#v", seen[0])
	}
}

func TestConversationCreateProxiesToRustAPIAndBridgesLocalID(t *testing.T) {
	gin.SetMode(gin.TestMode)
	var seen seenConfigProxyRequest
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		seen = seenConfigProxyRequest{Method: r.Method, Path: r.URL.Path, Query: r.URL.RawQuery, Body: string(body)}
		w.Header().Set("Content-Type", "application/json")
		if r.Method != http.MethodPost || r.URL.Path != "/api/conversations" {
			http.NotFound(w, r)
			return
		}
		_, _ = w.Write([]byte(`{"id":"conv-rust","title":"Rust Conversation","projectId":"","pinned":false,"createdAt":"2026-06-26T00:00:00Z","updatedAt":"2026-06-26T00:00:00Z","messages":[]}`))
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	db, err := database.NewDB(filepath.Join(t.TempDir(), "create.sqlite"), zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	defer db.Close()

	router := gin.New()
	h := NewConversationHandler(db, zap.NewNop())
	router.POST("/api/conversations", h.CreateConversation)

	body := `{"title":"Rust Conversation"}`
	req := httptest.NewRequest(http.MethodPost, "/api/conversations", bytes.NewBufferString(body))
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	router.ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}
	if seen.Method != http.MethodPost || seen.Path != "/api/conversations" || seen.Body != body {
		t.Fatalf("seen = %#v", seen)
	}
	conv, err := db.GetConversationLite("conv-rust")
	if err != nil {
		t.Fatalf("bridged conversation missing locally: %v", err)
	}
	if conv.Title != "Rust Conversation" {
		t.Fatalf("local bridged title = %q", conv.Title)
	}
}

func TestConversationDetailAndProcessDetailsProxyToRustAPIWithoutReadBackfill(t *testing.T) {
	gin.SetMode(gin.TestMode)
	var seen []seenConfigProxyRequest
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		seen = append(seen, seenConfigProxyRequest{Method: r.Method, Path: r.URL.Path, Query: r.URL.RawQuery, Body: string(body)})
		w.Header().Set("Content-Type", "application/json")
		switch {
		case r.Method == http.MethodGet && r.URL.Path == "/api/conversations/conv-local":
			_, _ = w.Write([]byte(`{"id":"conv-local","title":"PG Detail","pinned":false,"createdAt":"2026-06-26T00:00:00Z","updatedAt":"2026-06-26T00:00:00Z","messages":[]}`))
		case r.Method == http.MethodGet && r.URL.Path == "/api/conversations/conv-local/runtime-todos":
			_, _ = w.Write([]byte(`{"conversationId":"conv-local","todos":[{"itemId":"todo-1","content":"Inspect","status":"completed","position":0,"updatedAt":"2026-06-26T00:00:00Z"}]}`))
		case r.Method == http.MethodGet && strings.HasPrefix(r.URL.Path, "/api/messages/") && strings.HasSuffix(r.URL.Path, "/process-details"):
			_, _ = w.Write([]byte(`{"processDetails":[{"id":"pd-pg","messageId":"msg","conversationId":"conv-local","eventType":"progress","message":"pg","data":{},"createdAt":"2026-06-26T00:00:00Z"}]}`))
		default:
			http.NotFound(w, r)
		}
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	db, _, msg := newConversationProxyTestDB(t)
	defer db.Close()

	router := gin.New()
	h := NewConversationHandler(db, zap.NewNop())
	router.GET("/api/conversations/:id", h.GetConversation)
	router.GET("/api/conversations/:id/runtime-todos", h.GetRuntimeTodos)
	router.GET("/api/messages/:id/process-details", h.GetMessageProcessDetails)

	w := httptest.NewRecorder()
	router.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/api/conversations/conv-local?include_process_details=0", nil))
	if w.Code != http.StatusOK {
		t.Fatalf("detail status = %d, body = %s", w.Code, w.Body.String())
	}
	if len(seen) != 1 || seen[0].Method != http.MethodGet || seen[0].Path != "/api/conversations/conv-local" || seen[0].Query != "include_process_details=0" {
		t.Fatalf("detail seen = %#v", seen)
	}

	seen = nil
	w = httptest.NewRecorder()
	router.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/api/conversations/conv-local/runtime-todos", nil))
	if w.Code != http.StatusOK {
		t.Fatalf("runtime todos status = %d, body = %s", w.Code, w.Body.String())
	}
	if len(seen) != 1 || seen[0].Method != http.MethodGet || seen[0].Path != "/api/conversations/conv-local/runtime-todos" {
		t.Fatalf("runtime todos seen = %#v", seen)
	}

	seen = nil
	w = httptest.NewRecorder()
	router.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/api/messages/"+msg.ID+"/process-details", nil))
	if w.Code != http.StatusOK {
		t.Fatalf("process details status = %d, body = %s", w.Code, w.Body.String())
	}
	if len(seen) != 1 {
		t.Fatalf("process details seen len = %d, want only proxied GET: %#v", len(seen), seen)
	}
	if seen[0].Method != http.MethodGet || seen[0].Path != "/api/messages/"+msg.ID+"/process-details" {
		t.Fatalf("process detail GET = %#v", seen[0])
	}
}

func TestConversationMutationsProxyToRustAPIAndMirrorLocalSQLite(t *testing.T) {
	gin.SetMode(gin.TestMode)
	var seen []seenConfigProxyRequest
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		seen = append(seen, seenConfigProxyRequest{Method: r.Method, Path: r.URL.Path, Query: r.URL.RawQuery, Body: string(body)})
		w.Header().Set("Content-Type", "application/json")
		switch {
		case r.Method == http.MethodPut && r.URL.Path == "/api/conversations/conv-local":
			_, _ = w.Write([]byte(`{"id":"conv-local","title":"Renamed","pinned":false,"createdAt":"2026-06-26T00:00:00Z","updatedAt":"2026-06-26T00:00:00Z","messages":[]}`))
		case r.Method == http.MethodPut && r.URL.Path == "/api/conversations/conv-local/project":
			_, _ = w.Write([]byte(`{"success":true,"projectId":"project-local"}`))
		case r.Method == http.MethodDelete && r.URL.Path == "/api/conversations/conv-local":
			_, _ = w.Write([]byte(`{"message":"删除成功"}`))
		default:
			http.NotFound(w, r)
		}
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	db, _, _ := newConversationProxyTestDB(t)
	defer db.Close()
	project, err := db.CreateProject(&database.Project{Name: "Local Project"})
	if err != nil {
		t.Fatalf("CreateProject: %v", err)
	}

	router := gin.New()
	h := NewConversationHandler(db, zap.NewNop())
	router.PUT("/api/conversations/:id", h.UpdateConversation)
	router.PUT("/api/conversations/:id/project", h.SetConversationProject)
	router.DELETE("/api/conversations/:id", h.DeleteConversation)

	renameBody := `{"title":"Renamed"}`
	req := httptest.NewRequest(http.MethodPut, "/api/conversations/conv-local", bytes.NewBufferString(renameBody))
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	router.ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("rename status = %d, body = %s", w.Code, w.Body.String())
	}
	conv, err := db.GetConversationLite("conv-local")
	if err != nil {
		t.Fatalf("GetConversationLite after rename: %v", err)
	}
	if conv.Title != "Renamed" {
		t.Fatalf("local title = %q", conv.Title)
	}

	projectBody := `{"projectId":"` + project.ID + `"}`
	req = httptest.NewRequest(http.MethodPut, "/api/conversations/conv-local/project", bytes.NewBufferString(projectBody))
	req.Header.Set("Content-Type", "application/json")
	w = httptest.NewRecorder()
	router.ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("project status = %d, body = %s", w.Code, w.Body.String())
	}
	pid, err := db.GetConversationProjectID("conv-local")
	if err != nil {
		t.Fatalf("GetConversationProjectID: %v", err)
	}
	if pid != project.ID {
		t.Fatalf("local project id = %q, want %q", pid, project.ID)
	}

	req = httptest.NewRequest(http.MethodDelete, "/api/conversations/conv-local", nil)
	w = httptest.NewRecorder()
	router.ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("delete status = %d, body = %s", w.Code, w.Body.String())
	}
	if _, err := db.GetConversationLite("conv-local"); err == nil {
		t.Fatalf("conversation still exists locally after delete")
	}
	if len(seen) != 3 {
		t.Fatalf("seen len = %d, want 3 mutation proxies: %#v", len(seen), seen)
	}
	if seen[0].Method != http.MethodPut || seen[0].Path != "/api/conversations/conv-local" || seen[0].Body != renameBody {
		t.Fatalf("rename proxy = %#v", seen[0])
	}
	if seen[1].Method != http.MethodPut || seen[1].Path != "/api/conversations/conv-local/project" || seen[1].Body != projectBody {
		t.Fatalf("project proxy = %#v", seen[1])
	}
	if seen[2].Method != http.MethodDelete || seen[2].Path != "/api/conversations/conv-local" {
		t.Fatalf("delete proxy = %#v", seen[2])
	}
}
