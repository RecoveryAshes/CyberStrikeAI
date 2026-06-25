package agentruntime

import (
	"context"
	"errors"
	"io"
	"net"
	"os"
	"path/filepath"
	"runtime"
	"syscall"
	"testing"
	"time"

	agentruntimepb "cyberstrike-ai/internal/agentruntime/pb"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
)

type fakeRuntimeService struct {
	agentruntimepb.UnimplementedAgentRuntimeServiceServer
}

func (s fakeRuntimeService) Run(stream grpc.BidiStreamingServer[agentruntimepb.RuntimeCommand, agentruntimepb.RuntimeEvent]) error {
	cmd, err := stream.Recv()
	if err != nil {
		return err
	}
	if _, err := stream.Recv(); !errors.Is(err, io.EOF) {
		return err
	}
	events := []Event{
		{Type: "turn_started", ConversationID: cmd.ConversationId, RuntimeSessionID: "session-1", TurnID: "turn-1"},
		{Type: "assistant_delta", ConversationID: cmd.ConversationId, RuntimeSessionID: "session-1", TurnID: "turn-1", Delta: "hi", Accumulated: "hi"},
		{Type: "turn_completed", ConversationID: cmd.ConversationId, RuntimeSessionID: "session-1", TurnID: "turn-1", Response: "hi"},
		{Type: "command_completed", CommandID: cmd.CommandId, ConversationID: cmd.ConversationId, RuntimeSessionID: "session-1"},
	}
	for _, event := range events {
		pb, err := eventToProto(event)
		if err != nil {
			return err
		}
		if err := stream.Send(pb); err != nil {
			return err
		}
	}
	return nil
}

func (s fakeRuntimeService) Health(context.Context, *agentruntimepb.HealthRequest) (*agentruntimepb.HealthResponse, error) {
	return &agentruntimepb.HealthResponse{Ok: true, Message: "ok"}, nil
}

func (s fakeRuntimeService) ResumeApproval(req *agentruntimepb.ResumeApprovalRequest, stream grpc.ServerStreamingServer[agentruntimepb.RuntimeEvent]) error {
	event := Event{
		Type:             "approval_resolved",
		ConversationID:   req.ConversationId,
		RuntimeSessionID: req.RuntimeSessionId,
		TurnID:           "turn-1",
		RequestID:        req.RequestId,
		Decision:         req.Decision,
		Message:          req.Message,
	}
	pb, err := eventToProto(event)
	if err != nil {
		return err
	}
	return stream.Send(pb)
}

func TestGRPCRuntimeClientStartTurnReceivesEvents(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	server := grpc.NewServer()
	agentruntimepb.RegisterAgentRuntimeServiceServer(server, fakeRuntimeService{})
	done := make(chan error, 1)
	go func() {
		done <- server.Serve(listener)
	}()
	defer func() {
		server.Stop()
		<-done
	}()

	conn, err := grpc.Dial(listener.Addr().String(), grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer conn.Close()
	client := &GRPCRuntimeClient{
		conn:     conn,
		client:   agentruntimepb.NewAgentRuntimeServiceClient(conn),
		runs:     make(map[string]struct{}),
		endpoint: listener.Addr().String(),
	}

	var events []Event
	err = client.StartTurn(context.Background(), Command{
		Type:           "start_turn",
		ConversationID: "conv-1",
		Message:        "hello",
	}, func(event Event) error {
		events = append(events, event)
		return nil
	})
	if err != nil {
		t.Fatalf("StartTurn: %v", err)
	}
	if len(events) != 3 {
		t.Fatalf("events len = %d, want 3: %#v", len(events), events)
	}
	if events[2].Type != "turn_completed" || events[2].Response != "hi" {
		t.Fatalf("final event = %#v", events[2])
	}
}

func TestGRPCRuntimeClientResumeApprovalSendsComment(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	server := grpc.NewServer()
	agentruntimepb.RegisterAgentRuntimeServiceServer(server, fakeRuntimeService{})
	done := make(chan error, 1)
	go func() {
		done <- server.Serve(listener)
	}()
	defer func() {
		server.Stop()
		<-done
	}()

	conn, err := grpc.Dial(listener.Addr().String(), grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer conn.Close()
	client := &GRPCRuntimeClient{
		conn:     conn,
		client:   agentruntimepb.NewAgentRuntimeServiceClient(conn),
		runs:     make(map[string]struct{}),
		endpoint: listener.Addr().String(),
	}

	var events []Event
	err = client.ResumeApproval(context.Background(), Command{
		Type:             "approval_response",
		ConversationID:   "",
		RuntimeSessionID: "",
		RequestID:        "approval-1",
		Decision:         "approve",
		Message:          "looks good",
	}, func(event Event) error {
		events = append(events, event)
		return nil
	})
	if err != nil {
		t.Fatalf("ResumeApproval: %v", err)
	}
	if len(events) != 1 || events[0].Message != "looks good" || events[0].RequestID != "approval-1" {
		t.Fatalf("events = %#v", events)
	}
}

func TestGRPCRuntimeClientProcessOutlivesStartupContext(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell script process test is unix-specific")
	}
	dir := t.TempDir()
	binary := filepath.Join(dir, "fake-runtime.sh")
	script := `#!/bin/sh
echo agent_runtime_grpc_listen=127.0.0.1:65535
trap 'exit 0' TERM INT
while true; do sleep 1; done
`
	if err := os.WriteFile(binary, []byte(script), 0o755); err != nil {
		t.Fatalf("write fake runtime: %v", err)
	}
	client := &GRPCRuntimeClient{BinaryPath: binary}
	ctx, cancel := context.WithCancel(context.Background())
	endpoint, err := client.ensureProcess(ctx)
	if err != nil {
		t.Fatalf("ensureProcess: %v", err)
	}
	if endpoint != "127.0.0.1:65535" {
		t.Fatalf("endpoint = %q", endpoint)
	}
	client.mu.Lock()
	proc := client.proc
	client.mu.Unlock()
	if proc == nil || proc.Process == nil {
		t.Fatal("runtime process was not recorded")
	}
	cancel()
	time.Sleep(100 * time.Millisecond)
	if err := proc.Process.Signal(syscall.Signal(0)); err != nil {
		t.Fatalf("runtime process exited after startup context cancellation: %v", err)
	}
	if err := client.Close(); err != nil {
		t.Fatalf("close client: %v", err)
	}
}
