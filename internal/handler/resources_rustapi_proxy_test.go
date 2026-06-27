package handler

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"

	"cyberstrike-ai/internal/config"
	"cyberstrike-ai/internal/database"

	"github.com/gin-gonic/gin"
	"go.uber.org/zap"
)

func TestRolesEndpointProxiesToRustAPI(t *testing.T) {
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
		case r.Method == http.MethodPost && r.URL.Path == "/api/internal/roles":
			_, _ = w.Write([]byte(`{"ok":true}`))
		case r.Method == http.MethodGet && r.URL.Path == "/api/roles":
			_, _ = w.Write([]byte(`{"roles":[{"name":"Analyst","description":"PG role","enabled":true}]}`))
		default:
			http.NotFound(w, r)
		}
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	router := gin.New()
	cfg := &config.Config{
		Roles: map[string]config.RoleConfig{
			"Local": {Name: "Local", Description: "must not be served"},
		},
	}
	h := NewRoleHandler(cfg, "/must/not/be/read.yaml", zap.NewNop())
	router.GET("/api/roles", h.GetRoles)

	w := httptest.NewRecorder()
	router.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/api/roles", nil))
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}
	if len(seen) != 2 {
		t.Fatalf("seen len = %d, want sync then GET: %#v", len(seen), seen)
	}
	if seen[0].Method != http.MethodPost || seen[0].Path != "/api/internal/roles" {
		t.Fatalf("seen[0] = %#v, want POST /api/internal/roles", seen[0])
	}
	var upsert struct {
		Name    string          `json:"name"`
		Enabled bool            `json:"enabled"`
		Value   json.RawMessage `json:"value"`
	}
	if err := json.Unmarshal([]byte(seen[0].Body), &upsert); err != nil {
		t.Fatalf("decode role sync body: %v; body=%s", err, seen[0].Body)
	}
	if upsert.Name != "Local" || upsert.Enabled {
		t.Fatalf("role sync body = %#v", upsert)
	}
	if !json.Valid(upsert.Value) || string(upsert.Value) == "{}" {
		t.Fatalf("role sync value missing: %s", upsert.Value)
	}
	if seen[1].Method != http.MethodGet || seen[1].Path != "/api/roles" || seen[1].Query != "" {
		t.Fatalf("seen[1] = %#v, want GET /api/roles", seen[1])
	}
	if w.Body.String() != `{"roles":[{"name":"Analyst","description":"PG role","enabled":true}]}` {
		t.Fatalf("unexpected body: %s", w.Body.String())
	}
}

func TestRolesEndpointFailsWhenRustSyncFails(t *testing.T) {
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
		http.Error(w, "sync failed", http.StatusServiceUnavailable)
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	router := gin.New()
	cfg := &config.Config{
		Roles: map[string]config.RoleConfig{
			"Local": {Name: "Local", Description: "must sync"},
		},
	}
	h := NewRoleHandler(cfg, "/must/not/be/read.yaml", zap.NewNop())
	router.GET("/api/roles", h.GetRoles)

	w := httptest.NewRecorder()
	router.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/api/roles", nil))
	if w.Code != http.StatusBadGateway {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}
	if len(seen) != 1 || seen[0].Method != http.MethodPost || seen[0].Path != "/api/internal/roles" {
		t.Fatalf("seen = %#v, want only failed sync POST", seen)
	}
}

func TestProjectsEndpointProxiesToRustAPIWithQuery(t *testing.T) {
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
		case r.Method == http.MethodPost && r.URL.Path == "/api/internal/projects":
			_, _ = w.Write([]byte(`{"ok":true}`))
		case r.Method == http.MethodGet && r.URL.Path == "/api/projects":
			_, _ = w.Write([]byte(`{"projects":[{"id":"p1","name":"PG Project","status":"active","pinned":false}],"total":1,"limit":500,"offset":0}`))
		default:
			http.NotFound(w, r)
		}
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	db, err := database.NewDB(filepath.Join(t.TempDir(), "projects.sqlite"), zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	defer db.Close()
	created, err := db.CreateProject(&database.Project{
		Name:        "Local Project",
		Description: "sync me",
		ScopeJSON:   `{"target":"example.com"}`,
		Status:      "active",
		Pinned:      true,
	})
	if err != nil {
		t.Fatalf("CreateProject: %v", err)
	}

	router := gin.New()
	h := NewProjectHandler(db, zap.NewNop())
	router.GET("/api/projects", h.ListProjects)

	w := httptest.NewRecorder()
	router.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/api/projects?limit=500&search=acme", nil))
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}
	if len(seen) != 2 {
		t.Fatalf("seen len = %d, want sync then GET: %#v", len(seen), seen)
	}
	if seen[0].Method != http.MethodPost || seen[0].Path != "/api/internal/projects" {
		t.Fatalf("seen[0] = %#v, want POST /api/internal/projects", seen[0])
	}
	var upsertProject struct {
		ID          string `json:"id"`
		Name        string `json:"name"`
		Description string `json:"description"`
		ScopeJSON   string `json:"scope_json"`
		Status      string `json:"status"`
		Pinned      bool   `json:"pinned"`
	}
	if err := json.Unmarshal([]byte(seen[0].Body), &upsertProject); err != nil {
		t.Fatalf("decode project sync body: %v; body=%s", err, seen[0].Body)
	}
	if upsertProject.ID != created.ID || upsertProject.Name != "Local Project" || upsertProject.Description != "sync me" || upsertProject.ScopeJSON != `{"target":"example.com"}` || upsertProject.Status != "active" || !upsertProject.Pinned {
		t.Fatalf("project sync body = %#v", upsertProject)
	}
	if seen[1].Method != http.MethodGet || seen[1].Path != "/api/projects" || seen[1].Query != "limit=500&search=acme" {
		t.Fatalf("seen[1] = %#v, want GET /api/projects?limit=500&search=acme", seen[1])
	}
	if w.Body.String() != `{"projects":[{"id":"p1","name":"PG Project","status":"active","pinned":false}],"total":1,"limit":500,"offset":0}` {
		t.Fatalf("unexpected body: %s", w.Body.String())
	}
}

func TestProjectsEndpointFailsWhenRustSyncFails(t *testing.T) {
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
		http.Error(w, "sync failed", http.StatusServiceUnavailable)
	}))
	defer upstream.Close()
	t.Setenv("RUSTAPI_BASE_URL", upstream.URL)
	t.Setenv("RUSTAPI_TIMEOUT_SECONDS", "5")

	db, err := database.NewDB(filepath.Join(t.TempDir(), "projects.sqlite"), zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	defer db.Close()
	if _, err := db.CreateProject(&database.Project{Name: "Local Project"}); err != nil {
		t.Fatalf("CreateProject: %v", err)
	}

	router := gin.New()
	h := NewProjectHandler(db, zap.NewNop())
	router.GET("/api/projects", h.ListProjects)

	w := httptest.NewRecorder()
	router.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/api/projects?limit=500", nil))
	if w.Code != http.StatusBadGateway {
		t.Fatalf("status = %d, body = %s", w.Code, w.Body.String())
	}
	if len(seen) != 1 || seen[0].Method != http.MethodPost || seen[0].Path != "/api/internal/projects" {
		t.Fatalf("seen = %#v, want only failed sync POST", seen)
	}
}
