package database

import (
	"path/filepath"
	"testing"

	"go.uber.org/zap"
)

func TestAgentRuntimeSessionUpsertAndRead(t *testing.T) {
	db, err := NewDB(filepath.Join(t.TempDir(), "conversations.db"), zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	defer db.Close()

	conv, err := db.CreateConversation("agent_runtime", ConversationCreateMeta{})
	if err != nil {
		t.Fatalf("CreateConversation: %v", err)
	}

	if err := db.UpsertAgentRuntimeSession(conv.ID, "session-1", "", "turn-1", "active"); err != nil {
		t.Fatalf("UpsertAgentRuntimeSession active: %v", err)
	}
	got, err := db.GetAgentRuntimeSession(conv.ID)
	if err != nil {
		t.Fatalf("GetAgentRuntimeSession: %v", err)
	}
	if got == nil || got.RuntimeSessionID != "session-1" || got.ActiveTurnID != "turn-1" {
		t.Fatalf("unexpected session after active upsert: %#v", got)
	}

	if err := db.MarkAgentRuntimeTurnFinished(conv.ID, "session-1", "turn-1", "completed"); err != nil {
		t.Fatalf("MarkAgentRuntimeTurnFinished: %v", err)
	}
	got, err = db.GetAgentRuntimeSession(conv.ID)
	if err != nil {
		t.Fatalf("GetAgentRuntimeSession after finish: %v", err)
	}
	if got.LastTurnID != "turn-1" || got.ActiveTurnID != "" || got.StateSummary != "completed" {
		t.Fatalf("unexpected session after finish: %#v", got)
	}
}

func TestAgentRuntimeSessionMigratesLegacyCodexTable(t *testing.T) {
	db, err := NewDB(filepath.Join(t.TempDir(), "conversations.db"), zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	defer db.Close()

	conv, err := db.CreateConversation("legacy", ConversationCreateMeta{})
	if err != nil {
		t.Fatalf("CreateConversation: %v", err)
	}
	if _, err := db.Exec(`
		CREATE TABLE codex_runtime_sessions (
			conversation_id TEXT PRIMARY KEY,
			runtime_session_id TEXT NOT NULL,
			last_turn_id TEXT,
			active_turn_id TEXT,
			state_summary TEXT,
			created_at DATETIME NOT NULL,
			updated_at DATETIME NOT NULL
		)
	`); err != nil {
		t.Fatalf("create legacy table: %v", err)
	}
	if _, err := db.Exec(`
		INSERT INTO codex_runtime_sessions (
			conversation_id, runtime_session_id, last_turn_id, active_turn_id, state_summary, created_at, updated_at
		) VALUES (?, 'session-legacy', 'turn-old', 'turn-active', 'pending_approval', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
	`, conv.ID); err != nil {
		t.Fatalf("insert legacy row: %v", err)
	}
	if err := db.migrateLegacyAgentRuntimeSessions(); err != nil {
		t.Fatalf("migrateLegacyAgentRuntimeSessions: %v", err)
	}

	got, err := db.GetAgentRuntimeSession(conv.ID)
	if err != nil {
		t.Fatalf("GetAgentRuntimeSession legacy: %v", err)
	}
	if got == nil || got.RuntimeSessionID != "session-legacy" || got.ActiveTurnID != "turn-active" || got.StateSummary != "pending_approval" {
		t.Fatalf("unexpected migrated session: %#v", got)
	}
}
