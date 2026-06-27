package agentruntime

import (
	"bufio"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os/exec"
	"strings"
	"sync"
	"time"
)

type Command struct {
	Type             string                 `json:"type"`
	CommandID        string                 `json:"command_id,omitempty"`
	ConversationID   string                 `json:"conversation_id,omitempty"`
	RuntimeSessionID string                 `json:"runtime_session_id,omitempty"`
	Message          string                 `json:"message,omitempty"`
	Context          map[string]interface{} `json:"context,omitempty"`
	Reason           string                 `json:"reason,omitempty"`
	ContinueAfter    bool                   `json:"continue_after,omitempty"`
	RequestID        string                 `json:"request_id,omitempty"`
	Decision         string                 `json:"decision,omitempty"`
}

type Event struct {
	Type                    string                 `json:"type"`
	EventID                 string                 `json:"event_id,omitempty"`
	CommandID               string                 `json:"command_id,omitempty"`
	ConversationID          string                 `json:"conversation_id,omitempty"`
	RuntimeSessionID        string                 `json:"runtime_session_id,omitempty"`
	TurnID                  string                 `json:"turn_id,omitempty"`
	Delta                   string                 `json:"delta,omitempty"`
	Accumulated             string                 `json:"accumulated,omitempty"`
	Response                string                 `json:"response,omitempty"`
	Reason                  string                 `json:"reason,omitempty"`
	Message                 string                 `json:"message,omitempty"`
	Items                   []PlanItem             `json:"items,omitempty"`
	ToolCallID              string                 `json:"tool_call_id,omitempty"`
	ToolName                string                 `json:"tool_name,omitempty"`
	Arguments               map[string]interface{} `json:"arguments,omitempty"`
	Result                  string                 `json:"result,omitempty"`
	Error                   string                 `json:"error,omitempty"`
	RequestID               string                 `json:"request_id,omitempty"`
	Permission              string                 `json:"permission,omitempty"`
	Decision                string                 `json:"decision,omitempty"`
	Summary                 string                 `json:"summary,omitempty"`
	TaskID                  string                 `json:"task_id,omitempty"`
	Strategy                string                 `json:"strategy,omitempty"`
	InputMessageCount       int                    `json:"input_message_count,omitempty"`
	InputChars              int                    `json:"input_chars,omitempty"`
	ReplacementMessageCount int                    `json:"replacement_message_count,omitempty"`
	ArtifactPath            string                 `json:"artifact_path,omitempty"`
	RuntimeEventType        string                 `json:"runtime_event_type,omitempty"`
	RuntimeTraceJSON        string                 `json:"runtime_trace_json,omitempty"`
	PayloadJSON             string                 `json:"payload_json,omitempty"`
	OccurredAt              string                 `json:"occurred_at,omitempty"`
	Sequence                string                 `json:"sequence,omitempty"`
	AssistantMessageID      string                 `json:"assistant_message_id,omitempty"`
	Raw                     map[string]interface{} `json:"-"`
}

type PlanItem struct {
	ID       string `json:"id"`
	Step     string `json:"step"`
	Status   string `json:"status"`
	Priority string `json:"priority,omitempty"`
}

type Client struct {
	BinaryPath string
	WorkDir    string
}

type PersistentClient struct {
	BinaryPath string
	WorkDir    string

	mu      sync.Mutex
	writeMu sync.Mutex
	proc    *exec.Cmd
	stdin   io.WriteCloser
	done    chan struct{}
	doneErr error
	doneSet bool
	runs    map[string]chan Event
	started bool
}

func (c Client) StartTurn(ctx context.Context, cmd Command, onEvent func(Event) error) error {
	binary := strings.TrimSpace(c.BinaryPath)
	if binary == "" {
		return errors.New("agent runtime binary path is empty")
	}
	prepareCommand(&cmd)

	proc := exec.CommandContext(ctx, binary)
	if strings.TrimSpace(c.WorkDir) != "" {
		proc.Dir = c.WorkDir
	}

	stdin, err := proc.StdinPipe()
	if err != nil {
		return fmt.Errorf("open runtime stdin: %w", err)
	}
	stdout, err := proc.StdoutPipe()
	if err != nil {
		return fmt.Errorf("open runtime stdout: %w", err)
	}
	stderr, err := proc.StderrPipe()
	if err != nil {
		return fmt.Errorf("open runtime stderr: %w", err)
	}

	var stderrBuf strings.Builder
	var stderrMu sync.Mutex
	go func() {
		scanner := bufio.NewScanner(stderr)
		for scanner.Scan() {
			stderrMu.Lock()
			if stderrBuf.Len() < 8192 {
				stderrBuf.WriteString(scanner.Text())
				stderrBuf.WriteByte('\n')
			}
			stderrMu.Unlock()
		}
	}()

	if err := proc.Start(); err != nil {
		return fmt.Errorf("start agent runtime: %w", err)
	}

	if err := json.NewEncoder(stdin).Encode(cmd); err != nil {
		_ = stdin.Close()
		_ = proc.Wait()
		return fmt.Errorf("send runtime command: %w", err)
	}
	_ = stdin.Close()

	scanErr := scanEvents(stdout, func(event Event) error {
		if event.Type == "command_completed" {
			return nil
		}
		if onEvent == nil {
			return nil
		}
		return onEvent(event)
	})
	waitErr := proc.Wait()
	if scanErr != nil {
		return scanErr
	}
	if waitErr != nil {
		stderrMu.Lock()
		errText := strings.TrimSpace(stderrBuf.String())
		stderrMu.Unlock()
		if errText != "" {
			return fmt.Errorf("agent runtime exited: %w: %s", waitErr, errText)
		}
		return fmt.Errorf("agent runtime exited: %w", waitErr)
	}
	return nil
}

func (c *PersistentClient) StartTurn(ctx context.Context, cmd Command, onEvent func(Event) error) error {
	if c == nil {
		return errors.New("agent runtime persistent client is nil")
	}
	prepareCommand(&cmd)

	if err := c.ensureStarted(ctx); err != nil {
		return err
	}
	events, unregister, err := c.registerRun(cmd.ConversationID)
	if err != nil {
		return err
	}
	defer unregister()
	if err := c.writeCommand(ctx, cmd, false); err != nil {
		return fmt.Errorf("send runtime command: %w", err)
	}
	interruptSent := false

	for {
		if !interruptSent {
			select {
			case <-ctx.Done():
				interruptSent = true
				reason := ctx.Err().Error()
				if cause := context.Cause(ctx); cause != nil {
					reason = cause.Error()
				}
				_ = c.writeCommand(context.Background(), Command{
					Type:           "interrupt_turn",
					CommandID:      newCommandID(),
					ConversationID: cmd.ConversationID,
					Reason:         reason,
					ContinueAfter:  strings.Contains(strings.ToLower(reason), "continue"),
				}, true)
				continue
			default:
			}
		}

		if interruptSent {
			select {
			case event := <-events:
				if event.Type == "command_completed" {
					if event.CommandID == cmd.CommandID {
						return nil
					}
					continue
				}
				if onEvent != nil {
					if err := onEvent(event); err != nil {
						c.stop()
						return err
					}
				}
				continue
			default:
			}
		}

		select {
		case <-c.doneSignal():
			err := c.processDoneErr()
			if interruptSent {
				timer := time.NewTimer(100 * time.Millisecond)
				for {
					select {
					case event := <-events:
						if event.Type == "command_completed" {
							if !timer.Stop() {
								<-timer.C
							}
							if event.CommandID == cmd.CommandID {
								return nil
							}
							continue
						}
						if onEvent != nil {
							if cbErr := onEvent(event); cbErr != nil {
								if !timer.Stop() {
									<-timer.C
								}
								c.stop()
								return cbErr
							}
						}
					case <-timer.C:
						c.stop()
						if err != nil {
							return err
						}
						return ctx.Err()
					}
				}
			}
			c.stop()
			if err != nil {
				return err
			}
			if interruptSent {
				return ctx.Err()
			}
			return errors.New("agent runtime exited before command completed")
		case <-ctx.Done():
			if interruptSent {
				continue
			}
			interruptSent = true
			reason := ctx.Err().Error()
			if cause := context.Cause(ctx); cause != nil {
				reason = cause.Error()
			}
			_ = c.writeCommand(context.Background(), Command{
				Type:           "interrupt_turn",
				CommandID:      newCommandID(),
				ConversationID: cmd.ConversationID,
				Reason:         reason,
				ContinueAfter:  strings.Contains(strings.ToLower(reason), "continue"),
			}, true)
		case event := <-events:
			if event.Type == "command_completed" {
				if event.CommandID == cmd.CommandID {
					return nil
				}
				continue
			}
			if onEvent != nil {
				if err := onEvent(event); err != nil {
					c.stop()
					return err
				}
			}
		}
	}
}

func (c *PersistentClient) InterruptTurn(ctx context.Context, conversationID, reason string, continueAfter bool) error {
	return c.sendControl(ctx, Command{
		Type:           "interrupt_turn",
		CommandID:      newCommandID(),
		ConversationID: conversationID,
		Reason:         reason,
		ContinueAfter:  continueAfter,
	})
}

func (c *PersistentClient) Close() error {
	if c == nil {
		return nil
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.closeLocked()
}

func (c *PersistentClient) sendControl(ctx context.Context, cmd Command) error {
	if c == nil {
		return errors.New("agent runtime persistent client is nil")
	}
	prepareCommand(&cmd)
	if err := c.ensureStarted(ctx); err != nil {
		return err
	}
	if err := c.writeCommand(ctx, cmd, false); err != nil {
		return fmt.Errorf("send runtime command: %w", err)
	}
	return nil
}

func (c *PersistentClient) ensureStarted(ctx context.Context) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.started && c.proc != nil {
		return nil
	}
	select {
	case <-ctx.Done():
		return ctx.Err()
	default:
	}
	binary := strings.TrimSpace(c.BinaryPath)
	if binary == "" {
		return errors.New("agent runtime binary path is empty")
	}
	proc := exec.Command(binary)
	if strings.TrimSpace(c.WorkDir) != "" {
		proc.Dir = c.WorkDir
	}
	stdin, err := proc.StdinPipe()
	if err != nil {
		return fmt.Errorf("open runtime stdin: %w", err)
	}
	stdout, err := proc.StdoutPipe()
	if err != nil {
		_ = stdin.Close()
		return fmt.Errorf("open runtime stdout: %w", err)
	}
	stderr, err := proc.StderrPipe()
	if err != nil {
		_ = stdin.Close()
		return fmt.Errorf("open runtime stderr: %w", err)
	}
	if err := proc.Start(); err != nil {
		_ = stdin.Close()
		return fmt.Errorf("start agent runtime: %w", err)
	}

	c.proc = proc
	c.stdin = stdin
	c.done = make(chan struct{})
	c.doneErr = nil
	c.doneSet = false
	c.runs = make(map[string]chan Event)
	c.started = true

	go scanPersistentStdout(stdout, c.dispatchEvent, c.processDone)
	go waitPersistentProcess(proc, stderr, c.processDone)
	return nil
}

func (c *PersistentClient) registerRun(conversationID string) (<-chan Event, func(), error) {
	key := strings.TrimSpace(conversationID)
	if key == "" {
		return nil, nil, errors.New("agent runtime command conversation_id is empty")
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if !c.started || c.runs == nil {
		return nil, nil, errors.New("agent runtime process is not started")
	}
	if _, exists := c.runs[key]; exists {
		return nil, nil, fmt.Errorf("conversation already has an active runtime submission: %s", key)
	}
	events := make(chan Event, 128)
	c.runs[key] = events
	unregister := func() {
		c.mu.Lock()
		defer c.mu.Unlock()
		if current, ok := c.runs[key]; ok && current == events {
			delete(c.runs, key)
		}
	}
	return events, unregister, nil
}

func (c *PersistentClient) dispatchEvent(event Event) {
	key := strings.TrimSpace(event.ConversationID)
	c.mu.Lock()
	var targets []chan Event
	if key != "" {
		if events := c.runs[key]; events != nil {
			targets = append(targets, events)
		}
	}
	if len(targets) == 0 && event.Type == "command_completed" {
		for _, events := range c.runs {
			targets = append(targets, events)
		}
	}
	c.mu.Unlock()
	for _, events := range targets {
		select {
		case events <- event:
		default:
			events <- event
		}
	}
}

func (c *PersistentClient) doneSignal() <-chan struct{} {
	c.mu.Lock()
	done := c.done
	c.mu.Unlock()
	if done == nil {
		closed := make(chan struct{})
		close(closed)
		return closed
	}
	return done
}

func (c *PersistentClient) processDone(err error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.done == nil {
		return
	}
	if c.doneSet {
		return
	}
	c.doneErr = err
	c.doneSet = true
	close(c.done)
}

func (c *PersistentClient) processDoneErr() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.doneErr
}

func (c *PersistentClient) writeCommand(ctx context.Context, cmd Command, keepAliveOnCancel bool) error {
	c.writeMu.Lock()
	defer c.writeMu.Unlock()
	c.mu.Lock()
	stdin := c.stdin
	c.mu.Unlock()
	if stdin == nil {
		return errors.New("agent runtime stdin is not available")
	}
	done := make(chan error, 1)
	go func() {
		done <- json.NewEncoder(stdin).Encode(cmd)
	}()
	select {
	case <-ctx.Done():
		if !keepAliveOnCancel {
			c.stop()
		}
		return ctx.Err()
	case err := <-done:
		if err != nil {
			c.stop()
		}
		return err
	}
}

func scanPersistentStdout(stdout io.Reader, onEvent func(Event), done func(error)) {
	err := scanEvents(stdout, func(event Event) error {
		onEvent(event)
		return nil
	})
	if err != nil {
		done(err)
	}
}

func waitPersistentProcess(proc *exec.Cmd, stderr io.Reader, done func(error)) {
	var stderrBuf strings.Builder
	scanner := bufio.NewScanner(stderr)
	for scanner.Scan() {
		if stderrBuf.Len() < 8192 {
			stderrBuf.WriteString(scanner.Text())
			stderrBuf.WriteByte('\n')
		}
	}
	waitErr := proc.Wait()
	if waitErr != nil {
		errText := strings.TrimSpace(stderrBuf.String())
		if errText != "" {
			done(fmt.Errorf("agent runtime exited: %w: %s", waitErr, errText))
			return
		}
		done(fmt.Errorf("agent runtime exited: %w", waitErr))
		return
	}
	if scanErr := scanner.Err(); scanErr != nil {
		done(fmt.Errorf("read runtime stderr: %w", scanErr))
		return
	}
	done(nil)
}

func (c *PersistentClient) closeLocked() error {
	if !c.started {
		return nil
	}
	var err error
	if c.stdin != nil {
		err = c.stdin.Close()
	}
	c.stopLocked()
	return err
}

func (c *PersistentClient) stop() {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.stopLocked()
}

func (c *PersistentClient) stopLocked() {
	if c.proc != nil && c.proc.Process != nil {
		_ = c.proc.Process.Kill()
	}
	if c.done != nil && !c.doneSet {
		c.doneErr = errors.New("agent runtime process stopped")
		c.doneSet = true
		close(c.done)
	}
	c.proc = nil
	c.stdin = nil
	c.done = nil
	c.runs = nil
	c.started = false
}

func scanEvents(stdout io.Reader, onEvent func(Event) error) error {
	scanner := bufio.NewScanner(stdout)
	scanner.Buffer(make([]byte, 0, 64*1024), 2*1024*1024)
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if line == "" {
			continue
		}
		var raw map[string]interface{}
		if err := json.Unmarshal([]byte(line), &raw); err != nil {
			return fmt.Errorf("decode runtime event: %w", err)
		}
		var event Event
		if err := json.Unmarshal([]byte(line), &event); err != nil {
			return fmt.Errorf("decode runtime event body: %w", err)
		}
		event.Raw = raw
		if err := onEvent(event); err != nil {
			return err
		}
	}
	if err := scanner.Err(); err != nil {
		return fmt.Errorf("read runtime event: %w", err)
	}
	return nil
}

func prepareCommand(cmd *Command) {
	if cmd.Type == "" {
		cmd.Type = "start_turn"
	}
	if cmd.CommandID == "" {
		cmd.CommandID = newCommandID()
	}
	if cmd.Context == nil {
		cmd.Context = map[string]interface{}{}
	}
}

func newCommandID() string {
	var raw [8]byte
	if _, err := rand.Read(raw[:]); err != nil {
		return "cmd_fallback"
	}
	return "cmd_" + hex.EncodeToString(raw[:])
}
