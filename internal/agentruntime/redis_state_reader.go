package agentruntime

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/url"
	"strconv"
	"strings"
	"time"
)

const (
	defaultRedisPrefix      = "csai:agent_runtime:"
	defaultRedisDialTimeout = 2 * time.Second
	defaultReplayLimit      = 100
	maxReplayLimit          = 1000
)

type RedisStateReader struct {
	Addr   string
	Prefix string
}

func NewRedisStateReader(addr, prefix string) *RedisStateReader {
	prefix = strings.TrimSpace(prefix)
	if prefix == "" {
		prefix = defaultRedisPrefix
	}
	return &RedisStateReader{
		Addr:   strings.TrimSpace(addr),
		Prefix: prefix,
	}
}

func (r *RedisStateReader) GetRunState(ctx context.Context, conversationID string) (RunState, bool, error) {
	conversationID = strings.TrimSpace(conversationID)
	if conversationID == "" {
		return RunState{}, false, nil
	}
	if r == nil || strings.TrimSpace(r.Addr) == "" {
		return RunState{}, false, errors.New("agent runtime redis addr is empty")
	}
	raw, err := r.get(ctx, r.key("state:"+conversationID))
	if err != nil {
		return RunState{}, false, err
	}
	if strings.TrimSpace(raw) == "" {
		return RunState{}, false, nil
	}
	state, err := decodeRunState(raw)
	if err != nil {
		return RunState{}, false, err
	}
	return state, true, nil
}

func (r *RedisStateReader) ListRunStates(ctx context.Context) ([]RunState, error) {
	if r == nil || strings.TrimSpace(r.Addr) == "" {
		return nil, errors.New("agent runtime redis addr is empty")
	}
	conversations, err := r.smembers(ctx, r.key("active_states"))
	if err != nil {
		return nil, err
	}
	states, err := r.statesForConversations(ctx, conversations)
	if err != nil {
		return nil, err
	}
	if len(states) > 0 || len(conversations) > 0 {
		return filterActiveRunStates(states), nil
	}
	keys, err := r.scanKeys(ctx, r.key("state:*"))
	if err != nil {
		return nil, err
	}
	states = make([]RunState, 0, len(keys))
	for _, key := range keys {
		raw, err := r.get(ctx, key)
		if err != nil || strings.TrimSpace(raw) == "" {
			continue
		}
		state, err := decodeRunState(raw)
		if err == nil && isActiveRunStatus(state.Status) {
			states = append(states, state)
		}
	}
	return states, nil
}

func (r *RedisStateReader) ListEvents(ctx context.Context, conversationID, afterEventID string, limit int) ([]Event, error) {
	if r == nil || strings.TrimSpace(r.Addr) == "" {
		return nil, errors.New("agent runtime redis addr is empty")
	}
	conversationID = strings.TrimSpace(conversationID)
	if conversationID == "" {
		return nil, nil
	}
	limit = normalizeRedisReplayLimit(limit)
	start := "-"
	if after := strings.TrimSpace(afterEventID); after != "" {
		start = "(" + after
	}
	reply, err := r.command(ctx, "XRANGE", r.key("events:"+conversationID), start, "+", "COUNT", strconv.Itoa(limit))
	if err != nil {
		return nil, err
	}
	return decodeStreamEvents(reply)
}

func (r *RedisStateReader) statesForConversations(ctx context.Context, conversations []string) ([]RunState, error) {
	states := make([]RunState, 0, len(conversations))
	for _, conversationID := range conversations {
		conversationID = strings.TrimSpace(conversationID)
		if conversationID == "" {
			continue
		}
		raw, err := r.get(ctx, r.key("state:"+conversationID))
		if err != nil {
			continue
		}
		state, err := decodeRunState(raw)
		if err == nil {
			states = append(states, state)
		}
	}
	return states, nil
}

func (r *RedisStateReader) smembers(ctx context.Context, key string) ([]string, error) {
	reply, err := r.command(ctx, "SMEMBERS", key)
	if err != nil {
		return nil, err
	}
	values, ok := reply.([]interface{})
	if !ok {
		return nil, nil
	}
	out := make([]string, 0, len(values))
	for _, value := range values {
		if s := redisBulkString(value); strings.TrimSpace(s) != "" {
			out = append(out, s)
		}
	}
	return out, nil
}

func (r *RedisStateReader) get(ctx context.Context, key string) (string, error) {
	reply, err := r.command(ctx, "GET", key)
	if err != nil {
		return "", err
	}
	return redisBulkString(reply), nil
}

func (r *RedisStateReader) scanKeys(ctx context.Context, pattern string) ([]string, error) {
	var cursor int64
	var keys []string
	for {
		reply, err := r.command(ctx, "SCAN", strconv.FormatInt(cursor, 10), "MATCH", pattern, "COUNT", "100")
		if err != nil {
			return keys, err
		}
		values, ok := reply.([]interface{})
		if !ok || len(values) != 2 {
			return keys, nil
		}
		next, _ := strconv.ParseInt(redisBulkString(values[0]), 10, 64)
		if batch, ok := values[1].([]interface{}); ok {
			for _, value := range batch {
				if key := redisBulkString(value); key != "" {
					keys = append(keys, key)
				}
			}
		}
		if next == 0 {
			return keys, nil
		}
		cursor = next
	}
}

func (r *RedisStateReader) key(suffix string) string {
	prefix := strings.TrimSpace(r.Prefix)
	if prefix == "" {
		prefix = defaultRedisPrefix
	}
	return prefix + suffix
}

func (r *RedisStateReader) command(ctx context.Context, parts ...string) (interface{}, error) {
	addr := normalizeRedisTCPAddr(r.Addr)
	if addr == "" {
		return nil, errors.New("agent runtime redis addr is empty")
	}
	dialer := net.Dialer{}
	dialCtx := ctx
	var cancel context.CancelFunc
	if _, ok := ctx.Deadline(); !ok {
		dialCtx, cancel = context.WithTimeout(ctx, defaultRedisDialTimeout)
		defer cancel()
	}
	conn, err := dialer.DialContext(dialCtx, "tcp", addr)
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	if deadline, ok := ctx.Deadline(); ok {
		_ = conn.SetDeadline(deadline)
	} else {
		_ = conn.SetDeadline(time.Now().Add(defaultRedisDialTimeout))
	}
	if err := writeRedisArray(conn, parts...); err != nil {
		return nil, err
	}
	reader := bufio.NewReader(conn)
	return readRedisValue(reader)
}

func normalizeRedisTCPAddr(raw string) string {
	raw = strings.TrimSpace(raw)
	if raw == "" {
		return ""
	}
	if strings.Contains(raw, "://") {
		u, err := url.Parse(raw)
		if err != nil {
			return raw
		}
		if u.Host != "" {
			return u.Host
		}
	}
	return raw
}

func writeRedisArray(w io.Writer, parts ...string) error {
	var b strings.Builder
	fmt.Fprintf(&b, "*%d\r\n", len(parts))
	for _, part := range parts {
		fmt.Fprintf(&b, "$%d\r\n%s\r\n", len(part), part)
	}
	_, err := io.WriteString(w, b.String())
	return err
}

func readRedisValue(r *bufio.Reader) (interface{}, error) {
	prefix, err := r.ReadByte()
	if err != nil {
		return nil, err
	}
	switch prefix {
	case '+':
		line, err := readRedisLine(r)
		return string(line), err
	case '-':
		line, _ := readRedisLine(r)
		return nil, errors.New(string(line))
	case ':':
		line, err := readRedisLine(r)
		if err != nil {
			return nil, err
		}
		return strconv.ParseInt(string(line), 10, 64)
	case '$':
		line, err := readRedisLine(r)
		if err != nil {
			return nil, err
		}
		n, err := strconv.Atoi(string(line))
		if err != nil {
			return nil, err
		}
		if n < 0 {
			return nil, nil
		}
		buf := make([]byte, n+2)
		if _, err := io.ReadFull(r, buf); err != nil {
			return nil, err
		}
		return string(buf[:n]), nil
	case '*':
		line, err := readRedisLine(r)
		if err != nil {
			return nil, err
		}
		n, err := strconv.Atoi(string(line))
		if err != nil {
			return nil, err
		}
		if n < 0 {
			return nil, nil
		}
		out := make([]interface{}, 0, n)
		for i := 0; i < n; i++ {
			value, err := readRedisValue(r)
			if err != nil {
				return nil, err
			}
			out = append(out, value)
		}
		return out, nil
	default:
		return nil, fmt.Errorf("unsupported redis reply prefix %q", prefix)
	}
}

func readRedisLine(r *bufio.Reader) ([]byte, error) {
	line, err := r.ReadBytes('\n')
	if err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(bytes.TrimSuffix(line, []byte("\n")), []byte("\r")), nil
}

func redisBulkString(value interface{}) string {
	switch v := value.(type) {
	case string:
		return v
	case []byte:
		return string(v)
	case int64:
		return strconv.FormatInt(v, 10)
	default:
		return ""
	}
}

func decodeRunState(raw string) (RunState, error) {
	var state RunState
	var envelope struct {
		ConversationID     string `json:"conversation_id"`
		RuntimeSessionID   string `json:"runtime_session_id"`
		TurnID             string `json:"turn_id"`
		Status             string `json:"status"`
		Message            string `json:"message"`
		UpdatedAt          string `json:"updated_at"`
		AssistantMessageID string `json:"assistant_message_id"`
	}
	if err := json.Unmarshal([]byte(raw), &envelope); err != nil {
		return state, err
	}
	state.ConversationID = envelope.ConversationID
	state.RuntimeSessionID = envelope.RuntimeSessionID
	state.TurnID = envelope.TurnID
	state.Status = envelope.Status
	state.Message = envelope.Message
	state.UpdatedAt = envelope.UpdatedAt
	state.AssistantMessageID = envelope.AssistantMessageID
	return state, nil
}

func decodeStreamEvents(reply interface{}) ([]Event, error) {
	entries, ok := reply.([]interface{})
	if !ok {
		return nil, nil
	}
	events := make([]Event, 0, len(entries))
	for _, rawEntry := range entries {
		entry, ok := rawEntry.([]interface{})
		if !ok || len(entry) != 2 {
			continue
		}
		eventID := redisBulkString(entry[0])
		fields, ok := entry[1].([]interface{})
		if !ok {
			continue
		}
		fieldMap := redisFieldMap(fields)
		rawJSON := strings.TrimSpace(fieldMap["raw_json"])
		if rawJSON == "" {
			continue
		}
		event, err := decodeRuntimeEventJSON(rawJSON)
		if err != nil {
			continue
		}
		if event.EventID == "" {
			event.EventID = eventID
		}
		if event.RuntimeEventType == "" {
			event.RuntimeEventType = firstNonEmpty(fieldMap["runtime_event_type"], event.Type)
		}
		if event.RuntimeTraceJSON == "" {
			event.RuntimeTraceJSON = fieldMap["runtime_trace_json"]
		}
		if event.PayloadJSON == "" {
			event.PayloadJSON = fieldMap["payload_json"]
		}
		if event.OccurredAt == "" {
			event.OccurredAt = fieldMap["created_at_unix"]
		}
		if event.Sequence == "" {
			event.Sequence = eventID
		}
		if event.AssistantMessageID == "" {
			event.AssistantMessageID = fieldMap["assistant_message_id"]
		}
		events = append(events, event)
	}
	return events, nil
}

func redisFieldMap(values []interface{}) map[string]string {
	out := make(map[string]string, len(values)/2)
	for i := 0; i+1 < len(values); i += 2 {
		key := redisBulkString(values[i])
		if key == "" {
			continue
		}
		out[key] = redisBulkString(values[i+1])
	}
	return out
}

func decodeRuntimeEventJSON(raw string) (Event, error) {
	var event Event
	var envelope map[string]interface{}
	if err := json.Unmarshal([]byte(raw), &envelope); err != nil {
		return event, err
	}
	event.Raw = envelope
	event.Type = stringFromMap(envelope, "type")
	event.CommandID = stringFromMap(envelope, "command_id")
	event.ConversationID = stringFromMap(envelope, "conversation_id")
	event.RuntimeSessionID = stringFromMap(envelope, "runtime_session_id")
	event.TurnID = stringFromMap(envelope, "turn_id")
	event.Delta = stringFromMap(envelope, "delta")
	event.Accumulated = stringFromMap(envelope, "accumulated")
	event.Response = stringFromMap(envelope, "response")
	event.Reason = stringFromMap(envelope, "reason")
	event.Message = stringFromMap(envelope, "message")
	event.ToolCallID = stringFromMap(envelope, "tool_call_id")
	event.ToolName = stringFromMap(envelope, "tool_name")
	event.Result = stringFromMap(envelope, "result")
	event.Error = stringFromMap(envelope, "error")
	event.RequestID = stringFromMap(envelope, "request_id")
	event.Permission = stringFromMap(envelope, "permission")
	event.Decision = stringFromMap(envelope, "decision")
	event.Summary = stringFromMap(envelope, "summary")
	event.TaskID = stringFromMap(envelope, "task_id")
	event.Strategy = stringFromMap(envelope, "strategy")
	event.ArtifactPath = stringFromMap(envelope, "artifact_path")
	event.RuntimeEventType = stringFromMap(envelope, "runtime_event_type")
	event.RuntimeTraceJSON = stringFromMap(envelope, "runtime_trace_json")
	event.PayloadJSON = stringFromMap(envelope, "payload_json")
	event.OccurredAt = stringFromMap(envelope, "occurred_at")
	event.Sequence = stringFromMap(envelope, "sequence")
	event.AssistantMessageID = firstNonEmpty(stringFromMap(envelope, "assistant_message_id"), stringFromMap(envelope, "assistantMessageId"))
	event.InputMessageCount = intFromMap(envelope, "input_message_count")
	event.InputChars = intFromMap(envelope, "input_chars")
	event.ReplacementMessageCount = intFromMap(envelope, "replacement_message_count")
	if arguments, ok := envelope["arguments"].(map[string]interface{}); ok {
		event.Arguments = arguments
	}
	if rawItems, ok := envelope["items"].([]interface{}); ok {
		event.Items = decodePlanItems(rawItems)
	}
	return event, nil
}

func decodePlanItems(rawItems []interface{}) []PlanItem {
	items := make([]PlanItem, 0, len(rawItems))
	for _, raw := range rawItems {
		item, ok := raw.(map[string]interface{})
		if !ok {
			continue
		}
		items = append(items, PlanItem{
			ID:       stringFromMap(item, "id"),
			Step:     stringFromMap(item, "step"),
			Status:   stringFromMap(item, "status"),
			Priority: stringFromMap(item, "priority"),
		})
	}
	return items
}

func stringFromMap(m map[string]interface{}, key string) string {
	switch v := m[key].(type) {
	case string:
		return v
	case fmt.Stringer:
		return v.String()
	default:
		return ""
	}
}

func intFromMap(m map[string]interface{}, key string) int {
	switch v := m[key].(type) {
	case float64:
		return int(v)
	case int:
		return v
	case json.Number:
		n, _ := v.Int64()
		return int(n)
	default:
		return 0
	}
}

func filterActiveRunStates(states []RunState) []RunState {
	out := states[:0]
	for _, state := range states {
		if isActiveRunStatus(state.Status) {
			out = append(out, state)
		}
	}
	return out
}

func isActiveRunStatus(status string) bool {
	switch strings.TrimSpace(status) {
	case "running", "awaiting_approval", "cancelling":
		return true
	default:
		return false
	}
}

func normalizeRedisReplayLimit(limit int) int {
	if limit <= 0 {
		return defaultReplayLimit
	}
	if limit > maxReplayLimit {
		return maxReplayLimit
	}
	return limit
}
