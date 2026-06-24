package handler

import (
	"context"
	"fmt"
	"net/http"
	"regexp"
	"strings"
	"time"
	"unicode/utf8"

	"cyberstrike-ai/internal/openai"

	"go.uber.org/zap"
)

type conversationTitleGenerator func(ctx context.Context, userMessage, assistantResponse string) (string, error)

var isoDefaultConversationTitlePattern = regexp.MustCompile(`^(New session|Child session|新会话|子会话|New Chat|新对话)\s*[-·:]?\s*\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}`)

func isDefaultConversationTitle(title string) bool {
	t := strings.TrimSpace(title)
	if t == "" {
		return true
	}
	switch strings.ToLower(t) {
	case "new chat", "new conversation", "untitled", "新对话", "新会话", "未命名对话":
		return true
	}
	return isoDefaultConversationTitlePattern.MatchString(t)
}

func cleanGeneratedConversationTitle(text string) string {
	cleaned := strings.TrimSpace(text)
	if cleaned == "" {
		return ""
	}
	cleaned = regexp.MustCompile(`(?is)<think>.*?</think>`).ReplaceAllString(cleaned, "")
	cleaned = strings.TrimSpace(cleaned)
	if cleaned == "" {
		return ""
	}
	for _, line := range strings.Split(cleaned, "\n") {
		line = strings.TrimSpace(line)
		line = strings.TrimPrefix(line, "-")
		line = strings.TrimPrefix(line, "标题：")
		line = strings.TrimPrefix(line, "Title:")
		line = strings.TrimSpace(line)
		line = strings.Trim(line, "`\"'“”‘’")
		line = strings.TrimSpace(line)
		if line == "" {
			continue
		}
		line = strings.TrimRight(line, "。.!！?？：:")
		if utf8.RuneCountInString(line) > 60 {
			runes := []rune(line)
			line = string(runes[:57]) + "..."
		}
		return line
	}
	return ""
}

func (h *AgentHandler) maybeGenerateConversationTitle(ctx context.Context, conversationID, userMessage, assistantResponse string) {
	h.generateConversationTitleIfFirstUserMessage(ctx, conversationID, userMessage, assistantResponse, nil)
}

func (h *AgentHandler) maybeStartConversationTitleGeneration(conversationID, userMessage string, publish func(eventType, message string, data interface{})) {
	if h == nil {
		return
	}
	go h.generateConversationTitleIfFirstUserMessage(context.Background(), conversationID, userMessage, "", publish)
}

func (h *AgentHandler) generateConversationTitleIfFirstUserMessage(ctx context.Context, conversationID, userMessage, assistantResponse string, publish func(eventType, message string, data interface{})) {
	if h == nil || h.db == nil {
		return
	}
	conversationID = strings.TrimSpace(conversationID)
	if conversationID == "" {
		return
	}
	conv, err := h.db.GetConversationLite(conversationID)
	if err != nil {
		if h.logger != nil {
			h.logger.Warn("自动生成会话标题：读取会话失败", zap.String("conversationId", conversationID), zap.Error(err))
		}
		return
	}
	if conv == nil || !isDefaultConversationTitle(conv.Title) {
		return
	}
	originalTitle := conv.Title
	realUserMessages := 0
	firstUserMessage := ""
	for _, msg := range conv.Messages {
		if msg.Role == "user" && strings.TrimSpace(msg.Content) != "" {
			realUserMessages++
			if firstUserMessage == "" {
				firstUserMessage = strings.TrimSpace(msg.Content)
			}
		}
	}
	if realUserMessages != 1 {
		return
	}
	if strings.TrimSpace(userMessage) == "" {
		userMessage = firstUserMessage
	}
	if ctx == nil || ctx.Err() != nil {
		ctx = context.Background()
	}
	titleCtx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	generator := h.conversationTitleGenerator
	if generator == nil {
		generator = h.generateConversationTitleWithCurrentModel
	}
	title, err := generator(titleCtx, userMessage, assistantResponse)
	if err != nil {
		if h.logger != nil {
			h.logger.Warn("自动生成会话标题失败", zap.String("conversationId", conversationID), zap.Error(err))
		}
		return
	}
	title = cleanGeneratedConversationTitle(title)
	if title == "" || isDefaultConversationTitle(title) {
		return
	}
	updated, err := h.db.UpdateConversationTitleIfCurrent(conversationID, originalTitle, title)
	if err != nil {
		if h.logger != nil {
			h.logger.Warn("更新自动生成会话标题失败", zap.String("conversationId", conversationID), zap.String("title", title), zap.Error(err))
		}
		return
	}
	if updated && publish != nil {
		publish("conversation_title_updated", title, map[string]interface{}{
			"conversationId": conversationID,
			"title":          title,
		})
	}
}

func (h *AgentHandler) generateConversationTitleWithCurrentModel(ctx context.Context, userMessage, assistantResponse string) (string, error) {
	if h == nil || h.config == nil {
		return "", fmt.Errorf("server config is not loaded")
	}
	oa := h.config.OpenAI
	if strings.TrimSpace(oa.Model) == "" {
		return "", fmt.Errorf("openai model is empty")
	}
	userMessage = safeTitleContext(userMessage, 2400)
	assistantResponse = safeTitleContext(assistantResponse, 1600)
	if strings.TrimSpace(userMessage) == "" {
		return "", fmt.Errorf("user message is empty")
	}

	payload := map[string]interface{}{
		"model": oa.Model,
		"messages": []map[string]string{
			{
				"role":    "system",
				"content": "Generate a concise conversation title from the first user message. Reply with only the title, no quotes, no markdown, no explanation. Use the same language as the user when clear. Keep it under 12 words or 40 Chinese characters.",
			},
			{
				"role":    "user",
				"content": titleGenerationPrompt(userMessage, assistantResponse),
			},
		},
		"max_completion_tokens": 48,
	}

	client := openai.NewClient(&oa, &http.Client{Timeout: 30 * time.Second}, h.logger)
	callCtx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	var resp struct {
		Choices []struct {
			Message struct {
				Content string `json:"content"`
			} `json:"message"`
		} `json:"choices"`
	}
	if err := client.ChatCompletion(callCtx, payload, &resp); err != nil {
		return "", err
	}
	if len(resp.Choices) == 0 {
		return "", fmt.Errorf("title model response has no choices")
	}
	return resp.Choices[0].Message.Content, nil
}

func titleGenerationPrompt(userMessage, assistantResponse string) string {
	prompt := "Generate a title for this conversation:\n\nUser:\n" + userMessage
	if strings.TrimSpace(assistantResponse) != "" {
		prompt += "\n\nAssistant final answer:\n" + assistantResponse
	}
	return prompt
}

func safeTitleContext(text string, maxRunes int) string {
	text = strings.TrimSpace(text)
	if maxRunes <= 0 || utf8.RuneCountInString(text) <= maxRunes {
		return text
	}
	runes := []rune(text)
	return string(runes[:maxRunes])
}
