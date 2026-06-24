package database

import (
	"database/sql"
	"fmt"
	"time"
)

type AgentRuntimeSession struct {
	ConversationID   string
	RuntimeSessionID string
	LastTurnID       string
	ActiveTurnID     string
	StateSummary     string
	CreatedAt        time.Time
	UpdatedAt        time.Time
}

func (db *DB) GetAgentRuntimeSession(conversationID string) (*AgentRuntimeSession, error) {
	row := db.QueryRow(`
		SELECT conversation_id, runtime_session_id, COALESCE(last_turn_id, ''), COALESCE(active_turn_id, ''),
		       COALESCE(state_summary, ''), created_at, updated_at
		FROM agent_runtime_sessions
		WHERE conversation_id = ?
	`, conversationID)

	var session AgentRuntimeSession
	var createdAt, updatedAt string
	if err := row.Scan(
		&session.ConversationID,
		&session.RuntimeSessionID,
		&session.LastTurnID,
		&session.ActiveTurnID,
		&session.StateSummary,
		&createdAt,
		&updatedAt,
	); err != nil {
		if err == sql.ErrNoRows {
			return nil, nil
		}
		return nil, fmt.Errorf("查询 agent runtime session 失败: %w", err)
	}
	session.CreatedAt = parseDBTime(createdAt)
	session.UpdatedAt = parseDBTime(updatedAt)
	return &session, nil
}

func (db *DB) UpsertAgentRuntimeSession(conversationID, runtimeSessionID, lastTurnID, activeTurnID, stateSummary string) error {
	now := time.Now()
	_, err := db.Exec(`
		INSERT INTO agent_runtime_sessions (
			conversation_id, runtime_session_id, last_turn_id, active_turn_id, state_summary, created_at, updated_at
		) VALUES (?, ?, ?, ?, ?, ?, ?)
		ON CONFLICT(conversation_id) DO UPDATE SET
			runtime_session_id = excluded.runtime_session_id,
			last_turn_id = COALESCE(NULLIF(excluded.last_turn_id, ''), agent_runtime_sessions.last_turn_id),
			active_turn_id = excluded.active_turn_id,
			state_summary = COALESCE(NULLIF(excluded.state_summary, ''), agent_runtime_sessions.state_summary),
			updated_at = excluded.updated_at
	`, conversationID, runtimeSessionID, lastTurnID, activeTurnID, stateSummary, now, now)
	if err != nil {
		return fmt.Errorf("保存 agent runtime session 失败: %w", err)
	}
	return nil
}

func (db *DB) MarkAgentRuntimeTurnActive(conversationID, runtimeSessionID, turnID string) error {
	return db.UpsertAgentRuntimeSession(conversationID, runtimeSessionID, "", turnID, "active")
}

func (db *DB) MarkAgentRuntimeTurnFinished(conversationID, runtimeSessionID, turnID, stateSummary string) error {
	return db.UpsertAgentRuntimeSession(conversationID, runtimeSessionID, turnID, "", stateSummary)
}

func (db *DB) migrateLegacyAgentRuntimeSessions() error {
	var n int
	if err := db.QueryRow(`SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='codex_runtime_sessions'`).Scan(&n); err != nil {
		return err
	}
	if n == 0 {
		return nil
	}
	_, err := db.Exec(`
		INSERT OR IGNORE INTO agent_runtime_sessions (
			conversation_id, runtime_session_id, last_turn_id, active_turn_id, state_summary, created_at, updated_at
		)
		SELECT conversation_id, runtime_session_id, last_turn_id, active_turn_id, state_summary, created_at, updated_at
		FROM codex_runtime_sessions
	`)
	if err != nil {
		return fmt.Errorf("迁移 legacy agent runtime session 失败: %w", err)
	}
	return nil
}
