package handler

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"

	"cyberstrike-ai/internal/audit"
	"cyberstrike-ai/internal/database"
	"github.com/gin-gonic/gin"
	"go.uber.org/zap"
)

// ConversationHandler 对话处理器
type ConversationHandler struct {
	db     *database.DB
	logger *zap.Logger
	audit  *audit.Service
}

// SetAudit wires platform audit logging.
func (h *ConversationHandler) SetAudit(s *audit.Service) {
	h.audit = s
}

// NewConversationHandler 创建新的对话处理器
func NewConversationHandler(db *database.DB, logger *zap.Logger) *ConversationHandler {
	return &ConversationHandler{
		db:     db,
		logger: logger,
	}
}

func (h *ConversationHandler) log() *zap.Logger {
	if h != nil && h.logger != nil {
		return h.logger
	}
	return zap.NewNop()
}

// CreateConversationRequest 创建对话请求
type CreateConversationRequest struct {
	Title     string `json:"title"`
	ProjectID string `json:"projectId,omitempty"`
}

// SetConversationProjectRequest 设置对话所属项目
type SetConversationProjectRequest struct {
	ProjectID string `json:"projectId"` // 空字符串表示解除绑定
}

// CreateConversation 创建新对话
func (h *ConversationHandler) CreateConversation(c *gin.Context) {
	_, status, body, contentType, ok := h.proxyConversationToRust(c, "/api/conversations")
	if !ok {
		return
	}
	if status >= http.StatusOK && status < http.StatusMultipleChoices {
		h.bridgeRustCreatedConversationToLocal(c.Request.Context(), body)
	}
	h.writeRustProxyResponse(c, status, body, contentType)
}

// SetConversationProject 设置或清除对话绑定的项目
func (h *ConversationHandler) SetConversationProject(c *gin.Context) {
	id := c.Param("id")
	raw, status, body, contentType, ok := h.proxyConversationToRust(c, "/api/conversations/"+url.PathEscape(id)+"/project")
	if !ok {
		return
	}
	if status >= http.StatusOK && status < http.StatusMultipleChoices && h.db != nil {
		var req SetConversationProjectRequest
		if err := json.Unmarshal(raw, &req); err == nil {
			if err := h.db.SetConversationProjectID(id, req.ProjectID); err != nil {
				h.log().Debug("桥接会话项目到本地 SQLite 失败", zap.String("conversationId", id), zap.Error(err))
			}
		}
	}
	h.writeRustProxyResponse(c, status, body, contentType)
}

// ListConversations 列出对话
func (h *ConversationHandler) ListConversations(c *gin.Context) {
	h.proxyConversationToRustAndWrite(c, "/api/conversations")
}

// GetConversation 获取对话
func (h *ConversationHandler) GetConversation(c *gin.Context) {
	id := c.Param("id")
	h.proxyConversationToRustAndWrite(c, "/api/conversations/"+url.PathEscape(id))
}

// GetRuntimeTodos 获取会话级 Agent Runtime Todo 快照。
func (h *ConversationHandler) GetRuntimeTodos(c *gin.Context) {
	id := c.Param("id")
	if strings.TrimSpace(id) == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "conversation id required"})
		return
	}
	h.proxyConversationToRustAndWrite(c, "/api/conversations/"+url.PathEscape(id)+"/runtime-todos")
}

// GetMessageProcessDetails 获取指定消息的过程详情（按需加载）
func (h *ConversationHandler) GetMessageProcessDetails(c *gin.Context) {
	messageID := c.Param("id")
	if messageID == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "message id required"})
		return
	}
	h.proxyConversationToRustAndWrite(c, "/api/messages/"+url.PathEscape(messageID)+"/process-details")
}

// UpdateConversationRequest 更新对话请求
type UpdateConversationRequest struct {
	Title string `json:"title"`
}

// UpdateConversation 更新对话
func (h *ConversationHandler) UpdateConversation(c *gin.Context) {
	id := c.Param("id")
	raw, status, body, contentType, ok := h.proxyConversationToRust(c, "/api/conversations/"+url.PathEscape(id))
	if !ok {
		return
	}
	if status >= http.StatusOK && status < http.StatusMultipleChoices && h.db != nil {
		var req UpdateConversationRequest
		if err := json.Unmarshal(raw, &req); err == nil {
			if err := h.db.UpdateConversationTitle(id, req.Title); err != nil {
				h.log().Debug("桥接会话标题到本地 SQLite 失败", zap.String("conversationId", id), zap.Error(err))
			}
		}
	}
	h.writeRustProxyResponse(c, status, body, contentType)
}

// DeleteConversation 删除对话
func (h *ConversationHandler) DeleteConversation(c *gin.Context) {
	id := c.Param("id")
	_, status, body, contentType, ok := h.proxyConversationToRust(c, "/api/conversations/"+url.PathEscape(id))
	if !ok {
		return
	}
	if status >= http.StatusOK && status < http.StatusMultipleChoices && h.db != nil {
		if err := h.db.DeleteConversation(id); err != nil {
			h.log().Debug("桥接删除会话到本地 SQLite 失败", zap.String("conversationId", id), zap.Error(err))
		}
	}
	h.writeRustProxyResponse(c, status, body, contentType)
	if h.audit != nil && c.Writer.Status() >= http.StatusOK && c.Writer.Status() < http.StatusMultipleChoices {
		h.audit.Record(c, audit.Entry{
			Category:     "conversation",
			Action:       "delete",
			Result:       "success",
			ResourceType: "conversation",
			ResourceID:   id,
			Message:      "删除对话",
		})
	}
}

func (h *ConversationHandler) proxyConversationToRustAndWrite(c *gin.Context, path string) {
	_, status, body, contentType, ok := h.proxyConversationToRust(c, path)
	if !ok {
		return
	}
	h.writeRustProxyResponse(c, status, body, contentType)
}

func (h *ConversationHandler) proxyConversationToRust(c *gin.Context, path string) ([]byte, int, []byte, string, bool) {
	raw, err := c.GetRawData()
	if err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "读取请求体失败: " + err.Error()})
		return nil, 0, nil, "", false
	}
	c.Request.Body = io.NopCloser(bytes.NewReader(raw))
	c.Request.ContentLength = int64(len(raw))
	status, body, contentType, ok := proxyRequestToRust(c, nil, path, c.Request.URL.RawQuery, h.log(), "Rust API 会话/消息")
	c.Request.Body = io.NopCloser(bytes.NewReader(raw))
	c.Request.ContentLength = int64(len(raw))
	return raw, status, body, contentType, ok
}

func (h *ConversationHandler) writeRustProxyResponse(c *gin.Context, status int, body []byte, contentType string) {
	if strings.TrimSpace(contentType) == "" {
		contentType = "application/json; charset=utf-8"
	}
	c.Data(status, contentType, body)
}

func (h *ConversationHandler) syncConversationsToRust(ctx context.Context) error {
	if h == nil || h.db == nil {
		return nil
	}
	const batchSize = 500
	for offset := 0; ; offset += batchSize {
		convs, err := h.db.ListConversations(batchSize, offset, "", "updated_at")
		if err != nil {
			return fmt.Errorf("read local conversations: %w", err)
		}
		if len(convs) == 0 {
			return nil
		}
		for _, conv := range convs {
			if conv == nil {
				continue
			}
			if err := h.syncConversationToRustByID(ctx, conv.ID); err != nil {
				return fmt.Errorf("sync conversation %q: %w", conv.ID, err)
			}
		}
		if len(convs) < batchSize {
			return nil
		}
	}
}

func (h *ConversationHandler) syncConversationToRustByID(ctx context.Context, id string) error {
	if h == nil || h.db == nil || strings.TrimSpace(id) == "" {
		return nil
	}
	conv, err := h.db.GetConversation(id)
	if err != nil {
		return err
	}
	if err := h.syncConversationRecordToRust(ctx, conv); err != nil {
		return err
	}
	for i := range conv.Messages {
		msg := conv.Messages[i]
		if err := h.syncMessageRecordToRust(ctx, msg); err != nil {
			return err
		}
		if err := h.syncProcessDetailsToRustByMessageID(ctx, msg.ID); err != nil {
			return err
		}
	}
	return nil
}

func (h *ConversationHandler) syncProcessDetailsToRustByMessageID(ctx context.Context, messageID string) error {
	if h == nil || h.db == nil || strings.TrimSpace(messageID) == "" {
		return nil
	}
	details, err := h.db.GetProcessDetails(messageID)
	if err != nil {
		return err
	}
	details = database.DedupeConsecutiveProcessDetails(details)
	for _, detail := range details {
		if err := h.syncProcessDetailRecordToRust(ctx, detail); err != nil {
			return err
		}
	}
	return nil
}

func (h *ConversationHandler) syncConversationRecordToRust(ctx context.Context, conv *database.Conversation) error {
	if conv == nil || strings.TrimSpace(conv.ID) == "" {
		return nil
	}
	payload := map[string]interface{}{
		"id":        conv.ID,
		"title":     conv.Title,
		"projectId": conv.ProjectID,
		"pinned":    conv.Pinned,
		"createdAt": conv.CreatedAt.UTC().Format(time.RFC3339Nano),
		"updatedAt": conv.UpdatedAt.UTC().Format(time.RFC3339Nano),
	}
	return postJSONToRust(ctx, nil, "/api/internal/conversations", payload)
}

func (h *ConversationHandler) syncMessageRecordToRust(ctx context.Context, msg database.Message) error {
	if strings.TrimSpace(msg.ID) == "" || strings.TrimSpace(msg.ConversationID) == "" {
		return nil
	}
	payload := map[string]interface{}{
		"id":               msg.ID,
		"conversationId":   msg.ConversationID,
		"role":             msg.Role,
		"content":          msg.Content,
		"reasoningContent": msg.ReasoningContent,
		"mcpExecutionIds":  msg.MCPExecutionIDs,
		"createdAt":        msg.CreatedAt.UTC().Format(time.RFC3339Nano),
		"updatedAt":        msg.UpdatedAt.UTC().Format(time.RFC3339Nano),
	}
	return postJSONToRust(ctx, nil, "/api/internal/messages", payload)
}

func (h *ConversationHandler) syncProcessDetailRecordToRust(ctx context.Context, detail database.ProcessDetail) error {
	if strings.TrimSpace(detail.ID) == "" || strings.TrimSpace(detail.MessageID) == "" || strings.TrimSpace(detail.ConversationID) == "" {
		return nil
	}
	var data interface{}
	if strings.TrimSpace(detail.Data) != "" {
		if err := json.Unmarshal([]byte(detail.Data), &data); err != nil {
			h.log().Warn("解析过程详情数据失败", zap.String("processDetailId", detail.ID), zap.Error(err))
			data = nil
		}
	}
	payload := map[string]interface{}{
		"id":             detail.ID,
		"messageId":      detail.MessageID,
		"conversationId": detail.ConversationID,
		"eventType":      detail.EventType,
		"message":        detail.Message,
		"data":           data,
		"createdAt":      detail.CreatedAt.UTC().Format(time.RFC3339Nano),
	}
	return postJSONToRust(ctx, nil, "/api/internal/process-details", payload)
}

func (h *ConversationHandler) bridgeRustCreatedConversationToLocal(ctx context.Context, body []byte) {
	if h == nil || h.db == nil || len(body) == 0 {
		return
	}
	var conv struct {
		ID        string `json:"id"`
		Title     string `json:"title"`
		ProjectID string `json:"projectId"`
	}
	if err := json.Unmarshal(body, &conv); err != nil {
		h.log().Debug("解析 Rust 创建会话响应失败", zap.Error(err))
		return
	}
	if strings.TrimSpace(conv.ID) == "" {
		return
	}
	if _, err := h.db.GetConversationLite(conv.ID); err == nil {
		return
	}
	meta := database.ConversationCreateMeta{Source: "rustapi_bridge", ProjectID: strings.TrimSpace(conv.ProjectID)}
	title := strings.TrimSpace(conv.Title)
	if title == "" {
		title = "New Chat"
	}
	if _, err := h.db.CreateConversationWithID(conv.ID, "", title, meta); err != nil {
		h.log().Warn("桥接 Rust 会话到本地 SQLite 失败", zap.String("conversationId", conv.ID), zap.Error(err))
	}
	_ = ctx
}

// DeleteTurnRequest 删除一轮对话（POST /api/conversations/:id/delete-turn）
type DeleteTurnRequest struct {
	MessageID string `json:"messageId"`
}

// DeleteConversationTurn 删除锚点消息所在轮次（从该轮 user 到下一轮 user 之前），并清空 last_react_*。
func (h *ConversationHandler) DeleteConversationTurn(c *gin.Context) {
	conversationID := c.Param("id")
	if conversationID == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "conversation id required"})
		return
	}

	var req DeleteTurnRequest
	if err := c.ShouldBindJSON(&req); err != nil || req.MessageID == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "messageId required"})
		return
	}

	if _, err := h.db.GetConversation(conversationID); err != nil {
		c.JSON(http.StatusNotFound, gin.H{"error": "对话不存在"})
		return
	}

	deletedIDs, err := h.db.DeleteConversationTurn(conversationID, req.MessageID)
	if err != nil {
		h.logger.Warn("删除对话轮次失败",
			zap.String("conversationId", conversationID),
			zap.String("messageId", req.MessageID),
			zap.Error(err),
		)
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}

	if h.audit != nil {
		h.audit.RecordOK(c, "conversation", "delete_turn", "删除对话轮次", "conversation", conversationID, map[string]interface{}{
			"message_id": req.MessageID,
			"deleted":    len(deletedIDs),
		})
	}
	c.JSON(http.StatusOK, gin.H{
		"deletedMessageIds": deletedIDs,
		"message":           "ok",
	})
}
