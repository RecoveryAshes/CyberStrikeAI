package handler

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"

	"cyberstrike-ai/internal/config"
	"cyberstrike-ai/internal/database"

	"go.uber.org/zap"
)

func TestIsDefaultConversationTitle(t *testing.T) {
	defaults := []string{
		"",
		"New Chat",
		"new chat",
		"新对话",
		"未命名对话",
		"New session - 2026-06-24T13:45:12.123Z",
	}
	for _, title := range defaults {
		if !isDefaultConversationTitle(title) {
			t.Fatalf("isDefaultConversationTitle(%q) = false, want true", title)
		}
	}
	if isDefaultConversationTitle("分析本机 CPU 进程") {
		t.Fatalf("non-default title was classified as default")
	}
}

func TestCleanGeneratedConversationTitle(t *testing.T) {
	got := cleanGeneratedConversationTitle("<think>hidden</think>\n\"分析本机 CPU 进程。\"\nextra")
	if got != "分析本机 CPU 进程" {
		t.Fatalf("cleanGeneratedConversationTitle = %q", got)
	}

	long := strings.Repeat("很", 80)
	got = cleanGeneratedConversationTitle(long)
	if got == "" || !strings.HasSuffix(got, "...") {
		t.Fatalf("long generated title was not truncated: %q", got)
	}
}

func TestMaybeGenerateConversationTitleUpdatesOnlyFirstDefaultConversation(t *testing.T) {
	db := newTitleTestDB(t)
	conv, err := db.CreateConversation("New Chat", database.ConversationCreateMeta{})
	if err != nil {
		t.Fatalf("CreateConversation: %v", err)
	}
	if _, err := db.AddMessage(conv.ID, "user", "分析一下本机 CPU 进程", nil); err != nil {
		t.Fatalf("AddMessage user: %v", err)
	}
	if _, err := db.AddMessage(conv.ID, "assistant", "top 进程是 WindowServer", nil); err != nil {
		t.Fatalf("AddMessage assistant: %v", err)
	}

	var calls int
	h := &AgentHandler{
		db:     db,
		logger: zap.NewNop(),
		conversationTitleGenerator: func(ctx context.Context, userMessage, assistantResponse string) (string, error) {
			calls++
			if userMessage != "分析一下本机 CPU 进程" {
				t.Fatalf("userMessage = %q", userMessage)
			}
			if assistantResponse != "" {
				t.Fatalf("assistantResponse = %q", assistantResponse)
			}
			return "本机 CPU 进程分析", nil
		},
	}

	h.maybeGenerateConversationTitle(context.Background(), conv.ID, "分析一下本机 CPU 进程", "")
	if calls != 1 {
		t.Fatalf("title generator calls = %d, want 1", calls)
	}
	updated, err := db.GetConversationLite(conv.ID)
	if err != nil {
		t.Fatalf("GetConversationLite: %v", err)
	}
	if updated.Title != "本机 CPU 进程分析" {
		t.Fatalf("title = %q", updated.Title)
	}

	h.maybeGenerateConversationTitle(context.Background(), conv.ID, "第二轮", "回复")
	if calls != 1 {
		t.Fatalf("title generator called again after title update: %d", calls)
	}
}

func TestMaybeGenerateConversationTitleDoesNotOverwriteManualOrMultiTurnTitles(t *testing.T) {
	db := newTitleTestDB(t)

	manual, err := db.CreateConversation("手动标题", database.ConversationCreateMeta{})
	if err != nil {
		t.Fatalf("CreateConversation manual: %v", err)
	}
	if _, err := db.AddMessage(manual.ID, "user", "hello", nil); err != nil {
		t.Fatalf("AddMessage manual: %v", err)
	}

	multi, err := db.CreateConversation("New Chat", database.ConversationCreateMeta{})
	if err != nil {
		t.Fatalf("CreateConversation multi: %v", err)
	}
	if _, err := db.AddMessage(multi.ID, "user", "first", nil); err != nil {
		t.Fatalf("AddMessage first: %v", err)
	}
	if _, err := db.AddMessage(multi.ID, "assistant", "reply", nil); err != nil {
		t.Fatalf("AddMessage assistant: %v", err)
	}
	if _, err := db.AddMessage(multi.ID, "user", "second", nil); err != nil {
		t.Fatalf("AddMessage second: %v", err)
	}

	var calls int
	h := &AgentHandler{
		db:     db,
		logger: zap.NewNop(),
		conversationTitleGenerator: func(ctx context.Context, userMessage, assistantResponse string) (string, error) {
			calls++
			return "should not happen", nil
		},
	}

	h.maybeGenerateConversationTitle(context.Background(), manual.ID, "hello", "reply")
	h.maybeGenerateConversationTitle(context.Background(), multi.ID, "second", "reply")
	if calls != 0 {
		t.Fatalf("title generator calls = %d, want 0", calls)
	}
	gotManual, _ := db.GetConversationLite(manual.ID)
	if gotManual.Title != "手动标题" {
		t.Fatalf("manual title = %q", gotManual.Title)
	}
	gotMulti, _ := db.GetConversationLite(multi.ID)
	if gotMulti.Title != "New Chat" {
		t.Fatalf("multi title = %q", gotMulti.Title)
	}
}

func TestMaybeGenerateConversationTitleDoesNotOverwriteConcurrentRename(t *testing.T) {
	db := newTitleTestDB(t)
	conv, err := db.CreateConversation("New Chat", database.ConversationCreateMeta{})
	if err != nil {
		t.Fatalf("CreateConversation: %v", err)
	}
	if _, err := db.AddMessage(conv.ID, "user", "hello", nil); err != nil {
		t.Fatalf("AddMessage: %v", err)
	}
	h := &AgentHandler{
		db:     db,
		logger: zap.NewNop(),
		conversationTitleGenerator: func(ctx context.Context, userMessage, assistantResponse string) (string, error) {
			if err := db.UpdateConversationTitle(conv.ID, "手动改名"); err != nil {
				t.Fatalf("UpdateConversationTitle: %v", err)
			}
			return "模型标题", nil
		},
	}

	h.maybeGenerateConversationTitle(context.Background(), conv.ID, "hello", "")
	updated, err := db.GetConversationLite(conv.ID)
	if err != nil {
		t.Fatalf("GetConversationLite: %v", err)
	}
	if updated.Title != "手动改名" {
		t.Fatalf("title = %q, want 手动改名", updated.Title)
	}
}

func TestGenerateConversationTitleWithCurrentModelUsesConfiguredModel(t *testing.T) {
	var gotModel string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/chat/completions" {
			t.Fatalf("unexpected path: %s", r.URL.Path)
		}
		var payload struct {
			Model    string `json:"model"`
			Messages []struct {
				Role    string `json:"role"`
				Content string `json:"content"`
			} `json:"messages"`
		}
		if err := json.NewDecoder(r.Body).Decode(&payload); err != nil {
			t.Fatalf("decode payload: %v", err)
		}
		gotModel = payload.Model
		if len(payload.Messages) != 2 {
			t.Fatalf("messages len = %d, want 2", len(payload.Messages))
		}
		if !strings.Contains(payload.Messages[1].Content, "分析一下本机 CPU 进程") {
			t.Fatalf("title prompt missing user message: %q", payload.Messages[1].Content)
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"choices":[{"message":{"content":"CPU 进程分析"}}]}`))
	}))
	defer server.Close()

	h := &AgentHandler{
		config: &config.Config{
			OpenAI: config.OpenAIConfig{
				BaseURL: server.URL + "/v1",
				APIKey:  "test-key",
				Model:   "current-selected-model",
			},
		},
		logger: zap.NewNop(),
	}
	title, err := h.generateConversationTitleWithCurrentModel(context.Background(), "分析一下本机 CPU 进程", "WindowServer CPU 最高")
	if err != nil {
		t.Fatalf("generateConversationTitleWithCurrentModel: %v", err)
	}
	if gotModel != "current-selected-model" {
		t.Fatalf("model = %q, want current-selected-model", gotModel)
	}
	if title != "CPU 进程分析" {
		t.Fatalf("title = %q", title)
	}
}

func newTitleTestDB(t *testing.T) *database.DB {
	t.Helper()
	db, err := database.NewDB(filepath.Join(t.TempDir(), "title.sqlite"), zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	t.Cleanup(func() {
		_ = db.Close()
	})
	return db
}
