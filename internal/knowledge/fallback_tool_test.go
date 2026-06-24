package knowledge

import (
	"context"
	"database/sql"
	"strings"
	"testing"

	"cyberstrike-ai/internal/mcp"
	"cyberstrike-ai/internal/mcp/builtin"

	_ "github.com/mattn/go-sqlite3"
	"go.uber.org/zap"
)

func TestRegisterKnowledgeFallbackToolSearchesSQLite(t *testing.T) {
	db := newFallbackKnowledgeDB(t)
	server := mcp.NewServer(zap.NewNop())
	RegisterKnowledgeFallbackTool(server, db, zap.NewNop())

	riskTypes, _, err := server.CallTool(context.Background(), builtin.ToolListKnowledgeRiskTypes, map[string]interface{}{})
	if err != nil {
		t.Fatalf("list risk types: %v", err)
	}
	if !strings.Contains(mcp.ToolResultPlainText(riskTypes), "web") {
		t.Fatalf("risk types result = %#v", riskTypes)
	}

	result, _, err := server.CallTool(context.Background(), builtin.ToolSearchKnowledgeBase, map[string]interface{}{
		"query": "command injection",
	})
	if err != nil {
		t.Fatalf("search knowledge: %v", err)
	}
	text := mcp.ToolResultPlainText(result)
	if !strings.Contains(text, "Command Injection") || !strings.Contains(text, "command injection payloads") {
		t.Fatalf("search result = %s", text)
	}
}

func newFallbackKnowledgeDB(t *testing.T) *sql.DB {
	t.Helper()
	db, err := sql.Open("sqlite3", ":memory:")
	if err != nil {
		t.Fatalf("open sqlite: %v", err)
	}
	t.Cleanup(func() { _ = db.Close() })
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
	return db
}
