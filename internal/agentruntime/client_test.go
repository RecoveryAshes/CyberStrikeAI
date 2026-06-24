package agentruntime

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"
)

func TestClientStartTurnScansEvents(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell script test is unix-only")
	}
	dir := t.TempDir()
	script := filepath.Join(dir, "runtime.sh")
	if err := os.WriteFile(script, []byte(`#!/bin/sh
cat >/dev/null
printf '%s\n' '{"type":"turn_started","conversation_id":"c","runtime_session_id":"s","turn_id":"t"}'
printf '%s\n' '{"type":"assistant_progress_update","conversation_id":"c","runtime_session_id":"s","turn_id":"t","message":"checking state"}'
printf '%s\n' '{"type":"assistant_delta","conversation_id":"c","runtime_session_id":"s","turn_id":"t","delta":"hi","accumulated":"hi"}'
printf '%s\n' '{"type":"turn_completed","conversation_id":"c","runtime_session_id":"s","turn_id":"t","response":"hi"}'
`), 0o755); err != nil {
		t.Fatalf("write script: %v", err)
	}

	var events []Event
	err := (Client{BinaryPath: script}).StartTurn(context.Background(), Command{
		Type:           "start_turn",
		ConversationID: "c",
		Message:        "hello",
	}, func(event Event) error {
		events = append(events, event)
		return nil
	})
	if err != nil {
		t.Fatalf("StartTurn: %v", err)
	}
	if len(events) != 4 {
		t.Fatalf("events len = %d, want 4", len(events))
	}
	if events[1].Type != "assistant_progress_update" || events[1].Message != "checking state" {
		t.Fatalf("unexpected progress event: %#v", events[1])
	}
	if events[3].Type != "turn_completed" || events[3].Response != "hi" {
		t.Fatalf("unexpected final event: %#v", events[3])
	}
}

func TestPersistentClientReusesProcessAcrossCommands(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell script test is unix-only")
	}
	dir := t.TempDir()
	countFile := filepath.Join(dir, "starts")
	script := filepath.Join(dir, "runtime.sh")
	if err := os.WriteFile(script, []byte(fmt.Sprintf(`#!/bin/sh
echo started >> %s
while IFS= read -r line; do
  case "$line" in
    *'"type":"shutdown"'*) exit 0 ;;
  esac
  cmd=$(echo "$line" | sed 's/.*"command_id":"\([^"]*\)".*/\1/')
  printf '%%s\n' '{"type":"turn_started","conversation_id":"c","runtime_session_id":"s","turn_id":"t"}'
  printf '%%s\n' '{"type":"assistant_delta","conversation_id":"c","runtime_session_id":"s","turn_id":"t","delta":"hi","accumulated":"hi"}'
  printf '%%s\n' '{"type":"turn_completed","conversation_id":"c","runtime_session_id":"s","turn_id":"t","response":"hi"}'
  printf '%%s\n' "{\"type\":\"command_completed\",\"command_id\":\"$cmd\",\"conversation_id\":\"c\",\"runtime_session_id\":\"s\"}"
done
`, countFile)), 0o755); err != nil {
		t.Fatalf("write script: %v", err)
	}

	client := &PersistentClient{BinaryPath: script}
	defer client.Close()

	for i := 0; i < 2; i++ {
		var events []Event
		err := client.StartTurn(context.Background(), Command{
			Type:           "start_turn",
			ConversationID: "c",
			Message:        "hello",
		}, func(event Event) error {
			events = append(events, event)
			return nil
		})
		if err != nil {
			t.Fatalf("StartTurn %d: %v", i, err)
		}
		if len(events) != 3 {
			t.Fatalf("events len = %d, want 3", len(events))
		}
		if events[2].Type != "turn_completed" {
			t.Fatalf("unexpected final visible event: %#v", events[2])
		}
	}

	raw, err := os.ReadFile(countFile)
	if err != nil {
		t.Fatalf("read count: %v", err)
	}
	if got := strings.Count(string(raw), "started"); got != 1 {
		t.Fatalf("process starts = %d, want 1", got)
	}
}

func TestPersistentClientSendsInterruptOnContextCancel(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell script test is unix-only")
	}
	dir := t.TempDir()
	seenFile := filepath.Join(dir, "seen")
	script := filepath.Join(dir, "runtime.sh")
	if err := os.WriteFile(script, []byte(fmt.Sprintf(`#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"start_turn"'*)
      echo "$line" | sed 's/.*"command_id":"\([^"]*\)".*/\1/' > %s.cmd
      printf '%%s\n' '{"type":"turn_started","conversation_id":"c","runtime_session_id":"s","turn_id":"t"}'
      ;;
    *'"type":"interrupt_turn"'*)
      echo interrupted > %s
      cmd=$(cat %s.cmd)
      printf '%%s\n' '{"type":"turn_aborted","conversation_id":"c","runtime_session_id":"s","turn_id":"t","reason":"cancelled"}'
      printf '%%s\n' "{\"type\":\"command_completed\",\"command_id\":\"$cmd\",\"conversation_id\":\"c\",\"runtime_session_id\":\"s\"}"
      ;;
  esac
done
`, seenFile, seenFile, seenFile)), 0o755); err != nil {
		t.Fatalf("write script: %v", err)
	}

	client := &PersistentClient{BinaryPath: script}
	defer client.Close()
	ctx, cancel := context.WithCancelCause(context.Background())
	var events []Event
	done := make(chan error, 1)
	go func() {
		done <- client.StartTurn(ctx, Command{
			Type:           "start_turn",
			ConversationID: "c",
			Message:        "hello",
		}, func(event Event) error {
			events = append(events, event)
			return nil
		})
	}()

	time.Sleep(50 * time.Millisecond)
	cancel(errors.New("user cancelled"))
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("StartTurn after interrupt: %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("StartTurn did not finish after interrupt")
	}
	raw, err := os.ReadFile(seenFile)
	if err != nil {
		t.Fatalf("read seen interrupt: %v", err)
	}
	if strings.TrimSpace(string(raw)) != "interrupted" {
		t.Fatalf("interrupt marker = %q", raw)
	}
	if len(events) < 2 || events[len(events)-1].Type != "turn_aborted" {
		t.Fatalf("unexpected events: %#v", events)
	}
}

func TestPersistentClientAllowsConcurrentDifferentConversations(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell script test is unix-only")
	}
	dir := t.TempDir()
	script := filepath.Join(dir, "runtime.sh")
	if err := os.WriteFile(script, []byte(`#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"shutdown"'*) exit 0 ;;
  esac
  cmd=$(echo "$line" | sed 's/.*"command_id":"\([^"]*\)".*/\1/')
  conv=$(echo "$line" | sed 's/.*"conversation_id":"\([^"]*\)".*/\1/')
  (
    sleep 0.1
    printf '%s\n' "{\"type\":\"turn_started\",\"conversation_id\":\"$conv\",\"runtime_session_id\":\"s-$conv\",\"turn_id\":\"t-$conv\"}"
    printf '%s\n' "{\"type\":\"assistant_delta\",\"conversation_id\":\"$conv\",\"runtime_session_id\":\"s-$conv\",\"turn_id\":\"t-$conv\",\"delta\":\"hi-$conv\",\"accumulated\":\"hi-$conv\"}"
    printf '%s\n' "{\"type\":\"turn_completed\",\"conversation_id\":\"$conv\",\"runtime_session_id\":\"s-$conv\",\"turn_id\":\"t-$conv\",\"response\":\"hi-$conv\"}"
    printf '%s\n' "{\"type\":\"command_completed\",\"command_id\":\"$cmd\",\"conversation_id\":\"$conv\",\"runtime_session_id\":\"s-$conv\"}"
  ) &
done
`), 0o755); err != nil {
		t.Fatalf("write script: %v", err)
	}

	client := &PersistentClient{BinaryPath: script}
	defer client.Close()

	type result struct {
		conv   string
		events []Event
		err    error
	}
	results := make(chan result, 2)
	for _, conv := range []string{"c1", "c2"} {
		conv := conv
		go func() {
			var events []Event
			err := client.StartTurn(context.Background(), Command{
				Type:           "start_turn",
				ConversationID: conv,
				Message:        "hello",
			}, func(event Event) error {
				events = append(events, event)
				return nil
			})
			results <- result{conv: conv, events: events, err: err}
		}()
	}

	seen := map[string][]Event{}
	for i := 0; i < 2; i++ {
		select {
		case result := <-results:
			if result.err != nil {
				t.Fatalf("StartTurn %s: %v", result.conv, result.err)
			}
			seen[result.conv] = result.events
		case <-time.After(3 * time.Second):
			t.Fatal("concurrent StartTurn calls did not finish")
		}
	}
	for _, conv := range []string{"c1", "c2"} {
		events := seen[conv]
		if len(events) != 3 {
			t.Fatalf("%s events len = %d, want 3: %#v", conv, len(events), events)
		}
		for _, event := range events {
			if event.ConversationID != conv {
				t.Fatalf("%s received event for %s: %#v", conv, event.ConversationID, event)
			}
		}
		if events[2].Type != "turn_completed" || events[2].Response != "hi-"+conv {
			t.Fatalf("%s unexpected final event: %#v", conv, events[2])
		}
	}
}

func TestPersistentClientRejectsConcurrentSameConversation(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell script test is unix-only")
	}
	dir := t.TempDir()
	script := filepath.Join(dir, "runtime.sh")
	if err := os.WriteFile(script, []byte(`#!/bin/sh
cmd=""
conv=""
while IFS= read -r line; do
  case "$line" in
    *'"type":"start_turn"'*)
      cmd=$(echo "$line" | sed 's/.*"command_id":"\([^"]*\)".*/\1/')
      conv=$(echo "$line" | sed 's/.*"conversation_id":"\([^"]*\)".*/\1/')
      printf '%s\n' "{\"type\":\"turn_started\",\"conversation_id\":\"$conv\",\"runtime_session_id\":\"s\",\"turn_id\":\"t\"}"
      ;;
    *'"type":"interrupt_turn"'*)
      printf '%s\n' "{\"type\":\"turn_aborted\",\"conversation_id\":\"$conv\",\"runtime_session_id\":\"s\",\"turn_id\":\"t\",\"reason\":\"cancelled\"}"
      printf '%s\n' "{\"type\":\"command_completed\",\"command_id\":\"$cmd\",\"conversation_id\":\"$conv\",\"runtime_session_id\":\"s\"}"
      ;;
  esac
done
`), 0o755); err != nil {
		t.Fatalf("write script: %v", err)
	}

	client := &PersistentClient{BinaryPath: script}
	defer client.Close()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	started := make(chan struct{})
	done := make(chan error, 1)
	go func() {
		close(started)
		done <- client.StartTurn(ctx, Command{
			Type:           "start_turn",
			ConversationID: "same",
			Message:        "hello",
		}, nil)
	}()
	<-started
	time.Sleep(50 * time.Millisecond)

	err := client.StartTurn(context.Background(), Command{
		Type:           "start_turn",
		ConversationID: "same",
		Message:        "second",
	}, nil)
	if err == nil || !strings.Contains(err.Error(), "already has an active") {
		t.Fatalf("second StartTurn error = %v, want active conversation rejection", err)
	}
	cancel()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("first StartTurn did not unblock after cancel")
	}
}
