package knowledge

import (
	"context"
	"database/sql"
	"fmt"
	"strings"
	"time"

	"cyberstrike-ai/internal/mcp"
	"cyberstrike-ai/internal/mcp/builtin"

	"go.uber.org/zap"
)

// RegisterKnowledgeFallbackTool registers read-only SQLite knowledge tools when
// the full embedding retriever is not initialized. It keeps Agent Runtime MCP
// calls useful without enabling global knowledge indexing/retrieval services.
func RegisterKnowledgeFallbackTool(mcpServer *mcp.Server, db *sql.DB, logger *zap.Logger) {
	if mcpServer == nil || db == nil {
		return
	}
	manager := NewManager(db, "", logger)
	mcpServer.RegisterTool(mcp.Tool{
		Name:             builtin.ToolListKnowledgeRiskTypes,
		Description:      "获取知识库中所有可用的风险类型（risk_type）列表。",
		ShortDescription: "获取知识库风险类型列表",
		InputSchema: map[string]interface{}{
			"type":       "object",
			"properties": map[string]interface{}{},
			"required":   []string{},
		},
	}, func(ctx context.Context, args map[string]interface{}) (*mcp.ToolResult, error) {
		_ = ctx
		_ = args
		categories, err := manager.GetCategories()
		if err != nil {
			return textToolResult("获取风险类型列表失败: "+err.Error(), true), nil
		}
		if len(categories) == 0 {
			return textToolResult("知识库中暂无风险类型。", false), nil
		}
		var b strings.Builder
		b.WriteString(fmt.Sprintf("知识库中共有 %d 个风险类型：\n\n", len(categories)))
		for i, category := range categories {
			b.WriteString(fmt.Sprintf("%d. %s\n", i+1, category))
		}
		return textToolResult(b.String(), false), nil
	})

	mcpServer.RegisterTool(mcp.Tool{
		Name:             builtin.ToolSearchKnowledgeBase,
		Description:      "在知识库中搜索相关的安全知识。当前为 SQLite 关键词降级检索，不依赖 embedding 服务。",
		ShortDescription: "搜索知识库（关键词降级检索）",
		InputSchema: map[string]interface{}{
			"type": "object",
			"properties": map[string]interface{}{
				"query": map[string]interface{}{
					"type":        "string",
					"description": "搜索查询内容",
				},
				"risk_type": map[string]interface{}{
					"type":        "string",
					"description": "可选：指定风险类型",
				},
			},
			"required": []string{"query"},
		},
	}, func(ctx context.Context, args map[string]interface{}) (*mcp.ToolResult, error) {
		query, _ := args["query"].(string)
		query = strings.TrimSpace(query)
		if query == "" {
			return textToolResult("错误: 查询参数不能为空", true), nil
		}
		riskType, _ := args["risk_type"].(string)
		results, err := searchKnowledgeSQLiteFallback(ctx, db, query, strings.TrimSpace(riskType), 5)
		if err != nil {
			return textToolResult("检索失败: "+err.Error(), true), nil
		}
		if len(results) == 0 {
			return textToolResult(fmt.Sprintf("未找到与查询 '%s' 相关的知识。", query), false), nil
		}
		var b strings.Builder
		b.WriteString(fmt.Sprintf("找到 %d 条相关知识片段：\n\n", len(results)))
		for i, item := range results {
			b.WriteString(fmt.Sprintf("--- 结果 %d ---\n", i+1))
			b.WriteString(fmt.Sprintf("来源: [%s] %s (ID: %s)\n", item.Category, item.Title, item.ID))
			b.WriteString("内容片段:\n")
			b.WriteString(truncateFallbackText(item.Content, 1200))
			b.WriteString("\n\n")
		}
		return textToolResult(b.String(), false), nil
	})
	if logger != nil {
		logger.Info("知识库降级 MCP 工具已注册")
	}
}

type sqliteKnowledgeFallbackItem struct {
	ID       string
	Title    string
	Category string
	Content  string
}

func searchKnowledgeSQLiteFallback(ctx context.Context, db *sql.DB, query, riskType string, limit int) ([]sqliteKnowledgeFallbackItem, error) {
	if db == nil {
		return nil, fmt.Errorf("知识库数据库未初始化")
	}
	if limit <= 0 {
		limit = 5
	}
	if ctx == nil {
		ctx = context.Background()
	}
	ctx, cancel := context.WithTimeout(ctx, 3*time.Second)
	defer cancel()

	patterns := fallbackSearchPatterns(query)
	args := make([]interface{}, 0, len(patterns)*3+2)
	for _, pattern := range patterns {
		args = append(args, pattern, pattern, pattern)
	}
	riskClause := ""
	if riskType != "" {
		riskClause = " AND TRIM(i.category) = TRIM(?) COLLATE NOCASE"
		args = append(args, riskType)
	}
	args = append(args, limit)
	rows, err := db.QueryContext(ctx, fallbackSearchSQL(len(patterns), riskClause), args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var out []sqliteKnowledgeFallbackItem
	for rows.Next() {
		var item sqliteKnowledgeFallbackItem
		if err := rows.Scan(&item.ID, &item.Title, &item.Category, &item.Content); err != nil {
			return nil, err
		}
		out = append(out, item)
	}
	return out, rows.Err()
}

// SearchSQLiteFallbackSnippets returns compact snippet maps for Agent Runtime
// context injection when the vector retriever is not initialized.
func SearchSQLiteFallbackSnippets(ctx context.Context, db *sql.DB, query string, limit int) ([]map[string]interface{}, error) {
	items, err := searchKnowledgeSQLiteFallback(ctx, db, query, "", limit)
	if err != nil {
		return nil, err
	}
	out := make([]map[string]interface{}, 0, len(items))
	for _, item := range items {
		out = append(out, map[string]interface{}{
			"id":       item.ID,
			"title":    item.Title,
			"category": item.Category,
			"content":  truncateFallbackText(item.Content, 1200),
		})
	}
	return out, nil
}

func fallbackSearchPatterns(query string) []string {
	addPattern := func(out []string, seen map[string]struct{}, term string) []string {
		term = strings.TrimSpace(term)
		if term == "" {
			return out
		}
		pattern := "%" + term + "%"
		key := strings.ToLower(pattern)
		if _, ok := seen[key]; ok {
			return out
		}
		seen[key] = struct{}{}
		return append(out, pattern)
	}
	seen := make(map[string]struct{})
	patterns := addPattern(nil, seen, query)
	for _, token := range strings.FieldsFunc(query, func(r rune) bool {
		return r == ' ' || r == '\t' || r == '\n' || r == '\r' || r == ',' || r == ';' || r == '，' || r == '；' || r == '/' || r == '\\'
	}) {
		if len([]rune(token)) < 2 {
			continue
		}
		patterns = addPattern(patterns, seen, token)
	}
	if len(patterns) == 0 {
		return []string{"%" + query + "%"}
	}
	return patterns
}

func fallbackSearchSQL(patternCount int, riskClause string) string {
	if patternCount <= 0 {
		patternCount = 1
	}
	conditions := make([]string, 0, patternCount)
	for i := 0; i < patternCount; i++ {
		conditions = append(conditions, "(i.title LIKE ? COLLATE NOCASE OR i.category LIKE ? COLLATE NOCASE OR COALESCE(e.chunk_text, i.content, '') LIKE ? COLLATE NOCASE)")
	}
	return fmt.Sprintf(`
SELECT
  i.id,
  i.title,
  i.category,
  COALESCE(NULLIF(e.chunk_text, ''), i.content, '') AS content
FROM knowledge_base_items i
LEFT JOIN knowledge_embeddings e ON e.item_id = i.id
WHERE (%s)%s
GROUP BY i.id
ORDER BY i.updated_at DESC
LIMIT ?`, strings.Join(conditions, " OR "), riskClause)
}

func textToolResult(text string, isError bool) *mcp.ToolResult {
	return &mcp.ToolResult{
		Content: []mcp.Content{{Type: "text", Text: text}},
		IsError: isError,
	}
}

func truncateFallbackText(s string, maxRunes int) string {
	if maxRunes <= 0 || len([]rune(s)) <= maxRunes {
		return s
	}
	out := string([]rune(s)[:maxRunes])
	return out + "..."
}
