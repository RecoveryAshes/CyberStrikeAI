package database

import (
	"path/filepath"
	"testing"

	"go.uber.org/zap"
)

func TestUpdateConversationTitleIfCurrent(t *testing.T) {
	db, err := NewDB(filepath.Join(t.TempDir(), "conversation-title.sqlite"), zap.NewNop())
	if err != nil {
		t.Fatalf("NewDB: %v", err)
	}
	defer db.Close()

	conv, err := db.CreateConversation("New Chat", ConversationCreateMeta{})
	if err != nil {
		t.Fatalf("CreateConversation: %v", err)
	}
	updated, err := db.UpdateConversationTitleIfCurrent(conv.ID, "Other", "模型标题")
	if err != nil {
		t.Fatalf("UpdateConversationTitleIfCurrent wrong current: %v", err)
	}
	if updated {
		t.Fatalf("updated = true, want false")
	}
	got, err := db.GetConversationLite(conv.ID)
	if err != nil {
		t.Fatalf("GetConversationLite: %v", err)
	}
	if got.Title != "New Chat" {
		t.Fatalf("title changed unexpectedly: %q", got.Title)
	}

	updated, err = db.UpdateConversationTitleIfCurrent(conv.ID, "New Chat", "模型标题")
	if err != nil {
		t.Fatalf("UpdateConversationTitleIfCurrent: %v", err)
	}
	if !updated {
		t.Fatalf("updated = false, want true")
	}
	got, err = db.GetConversationLite(conv.ID)
	if err != nil {
		t.Fatalf("GetConversationLite after update: %v", err)
	}
	if got.Title != "模型标题" {
		t.Fatalf("title = %q", got.Title)
	}
}
