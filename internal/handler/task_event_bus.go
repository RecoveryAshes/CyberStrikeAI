package handler

import (
	"bytes"
	"encoding/json"
	"fmt"
	"strings"
	"sync"
)

// TaskEventBus 将主 SSE 连接上的事件镜像给后订阅的客户端（例如刷新页面后、HITL 审批通过需继续收事件）。
// 每个 payload 为完整 SSE 行： "data: {...}\n\n"
type TaskEventBus struct {
	mu         sync.RWMutex
	subs       map[string]map[*taskEventSub]struct{}
	globalSubs map[*taskEventSub]struct{}
	persist    func(conversationID string, line []byte)
}

type taskEventSub struct {
	mu     sync.Mutex
	ch     chan []byte
	closed bool
}

func (s *taskEventSub) sendNonBlocking(line []byte) bool {
	if s == nil {
		return false
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return false
	}
	select {
	case s.ch <- line:
		return true
	default:
		return false
	}
}

func (s *taskEventSub) closeOnce() {
	if s == nil {
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return
	}
	s.closed = true
	close(s.ch)
}

func NewTaskEventBus() *TaskEventBus {
	return &TaskEventBus{
		subs:       make(map[string]map[*taskEventSub]struct{}),
		globalSubs: make(map[*taskEventSub]struct{}),
	}
}

func (b *TaskEventBus) SetPersistHook(persist func(conversationID string, line []byte)) {
	if b == nil {
		return
	}
	b.mu.Lock()
	b.persist = persist
	b.mu.Unlock()
}

// Subscribe 注册订阅；cancel 时需调用 Unsubscribe。
func (b *TaskEventBus) Subscribe(conversationID string) (sub *taskEventSub, ch <-chan []byte) {
	chBuf := make(chan []byte, 256)
	sub = &taskEventSub{ch: chBuf}
	b.mu.Lock()
	if b.subs[conversationID] == nil {
		b.subs[conversationID] = make(map[*taskEventSub]struct{})
	}
	b.subs[conversationID][sub] = struct{}{}
	b.mu.Unlock()
	return sub, chBuf
}

// SubscribeAll 注册全局订阅。全局订阅不会因为某个会话结束而关闭，适合前端用一个
// EventSource 跟踪多个后台运行中的会话。
func (b *TaskEventBus) SubscribeAll() (sub *taskEventSub, ch <-chan []byte) {
	chBuf := make(chan []byte, 8192)
	sub = &taskEventSub{ch: chBuf}
	b.mu.Lock()
	b.globalSubs[sub] = struct{}{}
	b.mu.Unlock()
	return sub, chBuf
}

func (b *TaskEventBus) Unsubscribe(conversationID string, sub *taskEventSub) {
	if sub == nil {
		return
	}
	b.mu.Lock()
	m, ok := b.subs[conversationID]
	if !ok {
		b.mu.Unlock()
		return
	}
	delete(m, sub)
	if len(m) == 0 {
		delete(b.subs, conversationID)
	}
	b.mu.Unlock()
	sub.closeOnce()
}

func (b *TaskEventBus) UnsubscribeAll(sub *taskEventSub) {
	if sub == nil {
		return
	}
	b.mu.Lock()
	delete(b.globalSubs, sub)
	b.mu.Unlock()
	sub.closeOnce()
}

// Publish 非阻塞投递；慢消费者丢帧（HITL 场景以最新状态为准，丢帧可接受）。
func (b *TaskEventBus) Publish(conversationID string, line []byte) {
	if b == nil || conversationID == "" || len(line) == 0 {
		return
	}
	b.mu.RLock()
	m := b.subs[conversationID]
	subs := make([]*taskEventSub, 0, len(m)+len(b.globalSubs))
	for s := range m {
		subs = append(subs, s)
	}
	for s := range b.globalSubs {
		subs = append(subs, s)
	}
	b.mu.RUnlock()

	cp := append([]byte(nil), ensureTaskEventConversationID(conversationID, line)...)
	if persist := b.persistHook(); persist != nil {
		persistLine := append([]byte(nil), cp...)
		go persist(conversationID, persistLine)
	}
	for _, s := range subs {
		s.sendNonBlocking(cp)
	}
}

func (b *TaskEventBus) persistHook() func(conversationID string, line []byte) {
	if b == nil {
		return nil
	}
	b.mu.RLock()
	defer b.mu.RUnlock()
	return b.persist
}

// CloseConversation 任务结束时关闭该会话所有订阅 channel。
func (b *TaskEventBus) CloseConversation(conversationID string) {
	if b == nil || conversationID == "" {
		return
	}
	b.mu.Lock()
	m := b.subs[conversationID]
	delete(b.subs, conversationID)
	b.mu.Unlock()
	for sub := range m {
		sub.closeOnce()
	}
}

func ensureTaskEventConversationID(conversationID string, line []byte) []byte {
	conversationID = string(bytes.TrimSpace([]byte(conversationID)))
	if conversationID == "" || len(line) == 0 {
		return line
	}
	return ensureTaskEventDataString(line, "conversationId", conversationID)
}

func ensureTaskEventDataString(line []byte, key, value string) []byte {
	key = string(bytes.TrimSpace([]byte(key)))
	value = string(bytes.TrimSpace([]byte(value)))
	if key == "" || value == "" || len(line) == 0 {
		return line
	}
	trimmed := bytes.TrimSpace(line)
	if !bytes.HasPrefix(trimmed, []byte("data:")) {
		return line
	}
	payload := bytes.TrimSpace(bytes.TrimPrefix(trimmed, []byte("data:")))
	if len(payload) == 0 || bytes.Equal(payload, []byte("[DONE]")) {
		return line
	}
	var envelope map[string]interface{}
	if err := json.Unmarshal(payload, &envelope); err != nil {
		return line
	}
	data, ok := envelope["data"].(map[string]interface{})
	if !ok || data == nil {
		data = map[string]interface{}{}
		envelope["data"] = data
	}
	if existing, exists := data[key]; !exists || strings.TrimSpace(fmt.Sprint(existing)) == "" {
		data[key] = value
	}
	next, err := json.Marshal(envelope)
	if err != nil {
		return line
	}
	out := make([]byte, 0, len(next)+8)
	out = append(out, []byte("data: ")...)
	out = append(out, next...)
	out = append(out, '\n', '\n')
	return out
}
