package agentruntime

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os/exec"
	"strings"
	"sync"
	"time"

	agentruntimepb "cyberstrike-ai/internal/agentruntime/pb"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
)

type GRPCRuntimeClient struct {
	BinaryPath  string
	WorkDir     string
	ListenAddr  string
	RedisAddr   string
	RedisPrefix string

	startMu  sync.Mutex
	mu       sync.Mutex
	proc     *exec.Cmd
	procDone chan struct{}
	conn     *grpc.ClientConn
	client   agentruntimepb.AgentRuntimeServiceClient
	endpoint string
	runs     map[string]struct{}
}

func NewGRPCRuntimeClient(binaryPath, workDir, listenAddr, redisAddr, redisPrefix string) *GRPCRuntimeClient {
	return &GRPCRuntimeClient{
		BinaryPath:  binaryPath,
		WorkDir:     workDir,
		ListenAddr:  listenAddr,
		RedisAddr:   redisAddr,
		RedisPrefix: redisPrefix,
	}
}

func (c *GRPCRuntimeClient) StartTurn(ctx context.Context, cmd Command, onEvent func(Event) error) error {
	if cmd.Type == "" {
		cmd.Type = "start_turn"
	}
	return c.runCommand(ctx, cmd, onEvent)
}

func (c *GRPCRuntimeClient) ResumeApproval(ctx context.Context, cmd Command, onEvent func(Event) error) error {
	if cmd.Type == "" {
		cmd.Type = "approval_response"
	}
	registeredRun := false
	if strings.TrimSpace(cmd.ConversationID) != "" {
		if err := c.registerRun(cmd.ConversationID); err != nil {
			return err
		}
		registeredRun = true
	}
	if registeredRun {
		defer c.unregisterRun(cmd.ConversationID)
	}

	pbReq, err := resumeApprovalRequestFromCommand(cmd)
	if err != nil {
		return err
	}
	client, err := c.ensureClient(ctx)
	if err != nil {
		return err
	}
	stream, err := client.ResumeApproval(ctx, pbReq)
	if err != nil {
		return fmt.Errorf("resume runtime approval over grpc: %w", err)
	}
	for {
		pbEvent, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err != nil {
			return fmt.Errorf("receive runtime approval event over grpc: %w", err)
		}
		event, err := protoToEvent(pbEvent)
		if err != nil {
			return err
		}
		if event.Type == "command_completed" {
			continue
		}
		if onEvent != nil {
			if err := onEvent(event); err != nil {
				return err
			}
		}
	}
}

func (c *GRPCRuntimeClient) InterruptTurn(ctx context.Context, conversationID, reason string, continueAfter bool) error {
	conversationID = strings.TrimSpace(conversationID)
	if conversationID == "" {
		return errors.New("agent runtime interrupt conversation_id is empty")
	}
	client, err := c.ensureClient(ctx)
	if err != nil {
		if fallbackErr := c.writeRedisCancelSignal(ctx, conversationID, reason, continueAfter); fallbackErr == nil {
			return nil
		}
		return err
	}
	_, err = client.InterruptTurn(ctx, &agentruntimepb.InterruptTurnRequest{
		ConversationId: conversationID,
		Reason:         reason,
		ContinueAfter:  continueAfter,
	})
	if err != nil {
		if fallbackErr := c.writeRedisCancelSignal(ctx, conversationID, reason, continueAfter); fallbackErr == nil {
			return nil
		}
		return fmt.Errorf("interrupt runtime turn over grpc: %w", err)
	}
	return nil
}

func (c *GRPCRuntimeClient) writeRedisCancelSignal(ctx context.Context, conversationID, reason string, continueAfter bool) error {
	redisAddr := strings.TrimSpace(c.RedisAddr)
	if redisAddr == "" {
		return errors.New("agent runtime redis addr is empty")
	}
	prefix := strings.TrimSpace(c.RedisPrefix)
	if prefix == "" {
		prefix = "csai:agent_runtime:"
	}
	if reason = strings.TrimSpace(reason); reason == "" {
		reason = "runtime turn interrupted"
	}
	payload := map[string]interface{}{
		"reason":         reason,
		"continue_after": continueAfter,
		"requested_at":   fmt.Sprintf("%d", time.Now().Unix()),
	}
	raw, err := json.Marshal(payload)
	if err != nil {
		return err
	}
	dialer := net.Dialer{}
	conn, err := dialer.DialContext(ctx, "tcp", redisAddr)
	if err != nil {
		return err
	}
	defer conn.Close()
	if deadline, ok := ctx.Deadline(); ok {
		_ = conn.SetDeadline(deadline)
	} else {
		_ = conn.SetDeadline(time.Now().Add(2 * time.Second))
	}
	return writeRedisCommand(conn, "SETEX", prefix+"cancel:"+conversationID, "600", string(raw))
}

func writeRedisCommand(conn net.Conn, parts ...string) error {
	var b strings.Builder
	fmt.Fprintf(&b, "*%d\r\n", len(parts))
	for _, part := range parts {
		fmt.Fprintf(&b, "$%d\r\n%s\r\n", len(part), part)
	}
	if _, err := io.WriteString(conn, b.String()); err != nil {
		return err
	}
	reply := make([]byte, 1)
	if _, err := io.ReadFull(conn, reply); err != nil {
		return err
	}
	if reply[0] == '-' {
		buf := make([]byte, 256)
		n, _ := conn.Read(buf)
		return fmt.Errorf("redis error: %s", strings.TrimSpace(string(buf[:n])))
	}
	return nil
}

func (c *GRPCRuntimeClient) IsStarted() bool {
	if c == nil {
		return false
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.client != nil || c.proc != nil || c.endpoint != ""
}

func (c *GRPCRuntimeClient) Close() error {
	c.mu.Lock()
	var err error
	if c.conn != nil {
		err = c.conn.Close()
	}
	proc := c.proc
	procDone := c.procDone
	if proc != nil && proc.Process != nil {
		_ = proc.Process.Kill()
	}
	c.conn = nil
	c.client = nil
	c.proc = nil
	c.procDone = nil
	c.endpoint = ""
	c.runs = nil
	c.mu.Unlock()
	if procDone != nil {
		select {
		case <-procDone:
		case <-time.After(2 * time.Second):
		}
	}
	return err
}

func (c *GRPCRuntimeClient) runCommand(ctx context.Context, cmd Command, onEvent func(Event) error) error {
	prepareCommand(&cmd)
	if err := c.registerRun(cmd.ConversationID); err != nil {
		return err
	}
	defer c.unregisterRun(cmd.ConversationID)

	client, err := c.ensureClient(ctx)
	if err != nil {
		return err
	}
	if err := ctx.Err(); err != nil {
		return err
	}
	streamCtx, streamCancel := context.WithCancel(context.Background())
	defer streamCancel()
	stream, err := client.Run(streamCtx)
	if err != nil {
		return fmt.Errorf("open runtime grpc stream: %w", err)
	}
	pbCmd, err := commandToProto(cmd)
	if err != nil {
		return err
	}
	if err := stream.Send(pbCmd); err != nil {
		return fmt.Errorf("send runtime command over grpc: %w", err)
	}
	if err := stream.CloseSend(); err != nil {
		return fmt.Errorf("close runtime command stream: %w", err)
	}

	interruptSent := make(chan struct{})
	interruptDone := make(chan struct{})
	defer func() {
		close(interruptDone)
	}()
	go func() {
		select {
		case <-ctx.Done():
			reason := ctx.Err().Error()
			if cause := context.Cause(ctx); cause != nil {
				reason = cause.Error()
			}
			_ = c.InterruptTurn(context.Background(), cmd.ConversationID, reason, strings.Contains(strings.ToLower(reason), "continue"))
			close(interruptSent)
			select {
			case <-time.After(10 * time.Second):
				streamCancel()
			case <-interruptDone:
			}
		case <-interruptDone:
		}
	}()
	for {
		pbEvent, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err != nil {
			select {
			case <-interruptSent:
				if ctx.Err() != nil {
					return ctx.Err()
				}
			default:
			}
			if ctx.Err() != nil {
				return ctx.Err()
			}
			return fmt.Errorf("receive runtime event over grpc: %w", err)
		}
		event, err := protoToEvent(pbEvent)
		if err != nil {
			return err
		}
		if event.Type == "command_completed" {
			if event.CommandID == cmd.CommandID {
				return nil
			}
			continue
		}
		if onEvent != nil {
			if err := onEvent(event); err != nil {
				return err
			}
		}
	}
}

func (c *GRPCRuntimeClient) registerRun(conversationID string) error {
	key := strings.TrimSpace(conversationID)
	if key == "" {
		return errors.New("agent runtime command conversation_id is empty")
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.runs == nil {
		c.runs = make(map[string]struct{})
	}
	if _, exists := c.runs[key]; exists {
		return fmt.Errorf("conversation already has an active runtime submission: %s", key)
	}
	c.runs[key] = struct{}{}
	return nil
}

func (c *GRPCRuntimeClient) unregisterRun(conversationID string) {
	key := strings.TrimSpace(conversationID)
	if key == "" {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	delete(c.runs, key)
}

func (c *GRPCRuntimeClient) ensureClient(ctx context.Context) (agentruntimepb.AgentRuntimeServiceClient, error) {
	c.startMu.Lock()
	defer c.startMu.Unlock()

	c.mu.Lock()
	if c.client != nil {
		client := c.client
		c.mu.Unlock()
		return client, nil
	}
	c.mu.Unlock()

	endpoint, err := c.ensureProcess(ctx)
	if err != nil {
		return nil, err
	}
	dialCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	conn, err := grpc.DialContext(dialCtx, endpoint, grpc.WithTransportCredentials(insecure.NewCredentials()), grpc.WithBlock())
	if err != nil {
		return nil, fmt.Errorf("dial agent runtime grpc %s: %w", endpoint, err)
	}
	client := agentruntimepb.NewAgentRuntimeServiceClient(conn)
	healthCtx, healthCancel := context.WithTimeout(ctx, 5*time.Second)
	defer healthCancel()
	if _, err := client.Health(healthCtx, &agentruntimepb.HealthRequest{}); err != nil {
		_ = conn.Close()
		return nil, fmt.Errorf("agent runtime grpc health check: %w", err)
	}

	c.mu.Lock()
	defer c.mu.Unlock()
	if c.client != nil {
		_ = conn.Close()
		return c.client, nil
	}
	c.conn = conn
	c.client = client
	return client, nil
}

func (c *GRPCRuntimeClient) ensureProcess(ctx context.Context) (string, error) {
	c.mu.Lock()
	if c.endpoint != "" && c.proc != nil {
		endpoint := c.endpoint
		c.mu.Unlock()
		return endpoint, nil
	}
	c.mu.Unlock()

	binary := strings.TrimSpace(c.BinaryPath)
	if binary == "" {
		return "", errors.New("agent runtime binary path is empty")
	}
	listen := strings.TrimSpace(c.ListenAddr)
	if listen == "" {
		listen = "127.0.0.1:0"
	}
	args := []string{"--transport", "grpc", "--listen", listen}
	if redisAddr := strings.TrimSpace(c.RedisAddr); redisAddr != "" {
		args = append(args, "--redis-addr", redisAddr)
	}
	if redisPrefix := strings.TrimSpace(c.RedisPrefix); redisPrefix != "" {
		args = append(args, "--redis-prefix", redisPrefix)
	}
	proc := exec.Command(binary, args...)
	if strings.TrimSpace(c.WorkDir) != "" {
		proc.Dir = c.WorkDir
	}
	stdout, err := proc.StdoutPipe()
	if err != nil {
		return "", fmt.Errorf("open runtime stdout: %w", err)
	}
	stderr, err := proc.StderrPipe()
	if err != nil {
		return "", fmt.Errorf("open runtime stderr: %w", err)
	}
	if err := proc.Start(); err != nil {
		return "", fmt.Errorf("start agent runtime grpc process: %w", err)
	}

	endpointCh := make(chan string, 1)
	errCh := make(chan error, 1)
	stderrBuf := &runtimeStderrBuffer{}
	go readGRPCEndpoint(stdout, endpointCh, errCh)
	go captureRuntimeStderr(stderr, stderrBuf)

	var endpoint string
	select {
	case endpoint = <-endpointCh:
	case err := <-errCh:
		_ = proc.Process.Kill()
		return "", withRuntimeStderr(err, stderrBuf)
	case <-ctx.Done():
		_ = proc.Process.Kill()
		return "", withRuntimeStderr(ctx.Err(), stderrBuf)
	case <-time.After(10 * time.Second):
		_ = proc.Process.Kill()
		return "", withRuntimeStderr(errors.New("timeout waiting for agent runtime grpc endpoint"), stderrBuf)
	}
	endpoint = normalizeGRPCEndpoint(endpoint)
	c.mu.Lock()
	defer c.mu.Unlock()
	c.proc = proc
	procDone := make(chan struct{})
	c.procDone = procDone
	c.endpoint = endpoint
	if c.runs == nil {
		c.runs = make(map[string]struct{})
	}
	go func() {
		_ = proc.Wait()
		close(procDone)
		c.mu.Lock()
		if c.proc == proc {
			c.proc = nil
			c.procDone = nil
			c.endpoint = ""
			if c.conn != nil {
				_ = c.conn.Close()
			}
			c.conn = nil
			c.client = nil
		}
		c.mu.Unlock()
	}()
	return endpoint, nil
}

func readGRPCEndpoint(stdout io.Reader, endpointCh chan<- string, errCh chan<- error) {
	scanner := bufio.NewScanner(stdout)
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if strings.HasPrefix(line, "agent_runtime_grpc_listen=") {
			endpointCh <- strings.TrimPrefix(line, "agent_runtime_grpc_listen=")
			return
		}
	}
	if err := scanner.Err(); err != nil {
		errCh <- fmt.Errorf("read runtime grpc endpoint: %w", err)
		return
	}
	errCh <- errors.New("agent runtime grpc process exited before reporting endpoint")
}

func captureRuntimeStderr(stderr io.Reader, buf *runtimeStderrBuffer) {
	scanner := bufio.NewScanner(stderr)
	for scanner.Scan() {
		buf.addLine(scanner.Text())
	}
}

type runtimeStderrBuffer struct {
	mu sync.Mutex
	b  strings.Builder
}

func (b *runtimeStderrBuffer) addLine(line string) {
	if b == nil {
		return
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.b.Len() >= 8192 {
		return
	}
	b.b.WriteString(line)
	b.b.WriteByte('\n')
}

func (b *runtimeStderrBuffer) String() string {
	if b == nil {
		return ""
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	return strings.TrimSpace(b.b.String())
}

func withRuntimeStderr(err error, buf *runtimeStderrBuffer) error {
	if err == nil {
		return nil
	}
	if stderr := buf.String(); stderr != "" {
		return fmt.Errorf("%w: %s", err, stderr)
	}
	return err
}

func normalizeGRPCEndpoint(endpoint string) string {
	endpoint = strings.TrimSpace(endpoint)
	host, port, err := net.SplitHostPort(endpoint)
	if err != nil {
		return endpoint
	}
	if host == "" || host == "::" || host == "[::]" {
		host = "127.0.0.1"
	}
	if strings.Contains(host, ":") && !strings.HasPrefix(host, "[") {
		host = "[" + host + "]"
	}
	return net.JoinHostPort(host, port)
}

func resumeApprovalRequestFromCommand(cmd Command) (*agentruntimepb.ResumeApprovalRequest, error) {
	prepareCommand(&cmd)
	contextJSON, err := marshalJSONMap(cmd.Context)
	if err != nil {
		return nil, fmt.Errorf("marshal runtime approval context: %w", err)
	}
	rawPB, err := commandToProto(cmd)
	if err != nil {
		return nil, err
	}
	return &agentruntimepb.ResumeApprovalRequest{
		CommandId:        cmd.CommandID,
		ConversationId:   cmd.ConversationID,
		RuntimeSessionId: cmd.RuntimeSessionID,
		RequestId:        cmd.RequestID,
		Decision:         cmd.Decision,
		Message:          cmd.Message,
		ContextJson:      contextJSON,
		RawJson:          rawPB.RawJson,
	}, nil
}
