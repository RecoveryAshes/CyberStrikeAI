package agentruntime

import "context"

// RuntimeClient is the transport boundary used by Go handlers. Implementations
// can use the legacy JSONL subprocess transport or the gRPC transport.
type RuntimeClient interface {
	StartTurn(ctx context.Context, cmd Command, onEvent func(Event) error) error
	InterruptTurn(ctx context.Context, conversationID, reason string, continueAfter bool) error
	ResumeApproval(ctx context.Context, cmd Command, onEvent func(Event) error) error
	IsStarted() bool
	Close() error
}

// StateReader reads the runtime state/event plane. In gRPC mode Rust writes
// these records to Redis, and the Go HTTP/SSE facade reads Redis directly.
type StateReader interface {
	GetRunState(ctx context.Context, conversationID string) (RunState, bool, error)
	ListRunStates(ctx context.Context) ([]RunState, error)
	ListEvents(ctx context.Context, conversationID, afterEventID string, limit int) ([]Event, error)
}

type RunState struct {
	ConversationID     string
	RuntimeSessionID   string
	TurnID             string
	Status             string
	Message            string
	UpdatedAt          string
	AssistantMessageID string
}

type JSONLPersistentClient struct {
	*PersistentClient
}

func NewJSONLPersistentClient(binaryPath, workDir string) *JSONLPersistentClient {
	return &JSONLPersistentClient{
		PersistentClient: &PersistentClient{
			BinaryPath: binaryPath,
			WorkDir:    workDir,
		},
	}
}

func (c *JSONLPersistentClient) ResumeApproval(ctx context.Context, cmd Command, onEvent func(Event) error) error {
	if cmd.Type == "" {
		cmd.Type = "approval_response"
	}
	return c.StartTurn(ctx, cmd, onEvent)
}

func (c *JSONLPersistentClient) IsStarted() bool {
	if c == nil || c.PersistentClient == nil {
		return false
	}
	c.PersistentClient.mu.Lock()
	defer c.PersistentClient.mu.Unlock()
	return c.PersistentClient.started
}
