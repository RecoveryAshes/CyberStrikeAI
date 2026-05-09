package multiagent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"unicode/utf8"

	"cyberstrike-ai/internal/einomcp"

	"github.com/cloudwego/eino/adk"
	"github.com/cloudwego/eino/schema"
	"go.uber.org/zap"
)

// normalizeStreamingDelta 将可能是“累计片段”的 chunk 归一化为“纯增量”。
// 一些模型/桥接层在流式过程中会重复发送已输出前缀，前端若直接 buffer+=chunk 会出现重复文本。
//
// 注意：与 internal/openai.normalizeStreamingDelta 保持一致。
func normalizeStreamingDelta(current, incoming string) (next, delta string) {
	if incoming == "" {
		return current, ""
	}
	if current == "" {
		return incoming, incoming
	}
	if strings.HasPrefix(incoming, current) && len(incoming) > len(current) {
		return incoming, incoming[len(current):]
	}
	if incoming == current && utf8.RuneCountInString(current) > 1 {
		return current, ""
	}
	return current + incoming, incoming
}

func isEinoIterationLimitError(err error) bool {
	if err == nil {
		return false
	}
	msg := strings.ToLower(strings.TrimSpace(err.Error()))
	if msg == "" {
		return false
	}
	return strings.Contains(msg, "max iteration") ||
		strings.Contains(msg, "maximum iteration") ||
		strings.Contains(msg, "maximum iterations") ||
		strings.Contains(msg, "iteration limit") ||
		strings.Contains(msg, "达到最大迭代")
}

// einoADKRunLoopArgs 将 Eino adk.Runner 事件循环从 RunDeepAgent / RunEinoSingleChatModelAgent 中抽出复用。
type einoADKRunLoopArgs struct {
	OrchMode             string
	OrchestratorName     string
	ConversationID       string
	Progress             func(eventType, message string, data interface{})
	Logger               *zap.Logger
	SnapshotMCPIDs       func() []string
	StreamsMainAssistant func(agent string) bool
	EinoRoleTag          func(agent string) string
	CheckpointDir        string

	McpIDsMu *sync.Mutex
	McpIDs   *[]string

	// ToolInvokeNotify 与 einomcp.ToolsFromDefinitions 共享：run loop 在迭代前 Set，MCP 桥 Fire 以补全 tool_result。
	ToolInvokeNotify *einomcp.ToolInvokeNotifyHolder

	DA adk.Agent

	// EmptyResponseMessage 当未捕获到助手正文时的占位（多代理与单代理文案不同）。
	EmptyResponseMessage string
}

func runEinoADKAgentLoop(ctx context.Context, args *einoADKRunLoopArgs, baseMsgs []adk.Message) (*RunResult, error) {
	if args == nil || args.DA == nil {
		return nil, fmt.Errorf("eino run loop: args 或 Agent 为空")
	}
	if args.McpIDs == nil {
		s := []string{}
		args.McpIDs = &s
	}
	if args.McpIDsMu == nil {
		args.McpIDsMu = &sync.Mutex{}
	}

	orchMode := args.OrchMode
	orchestratorName := args.OrchestratorName
	conversationID := args.ConversationID
	progress := args.Progress
	logger := args.Logger
	snapshotMCPIDs := args.SnapshotMCPIDs
	if snapshotMCPIDs == nil {
		snapshotMCPIDs = func() []string { return nil }
	}
	streamsMainAssistant := args.StreamsMainAssistant
	if streamsMainAssistant == nil {
		streamsMainAssistant = func(agent string) bool {
			return agent == "" || agent == orchestratorName
		}
	}
	einoRoleTag := args.EinoRoleTag
	if einoRoleTag == nil {
		einoRoleTag = func(agent string) string {
			if streamsMainAssistant(agent) {
				return "orchestrator"
			}
			return "sub"
		}
	}
	da := args.DA
	mcpIDsMu := args.McpIDsMu
	mcpIDs := args.McpIDs

	// panic recovery：防止 Eino 框架内部 panic 导致整个 goroutine 崩溃、连接无法正常关闭。
	defer func() {
		if r := recover(); r != nil {
			if logger != nil {
				logger.Error("eino runner panic recovered", zap.Any("recover", r), zap.Stack("stack"))
			}
			if progress != nil {
				progress("error", fmt.Sprintf("Internal error: %v / 内部错误: %v", r, r), map[string]interface{}{
					"conversationId": conversationID,
					"source":         "eino",
				})
			}
		}
	}()

	var lastAssistant string
	var lastPlanExecuteExecutor string
	msgs := append([]adk.Message(nil), baseMsgs...)
	runAccumulatedMsgs := append([]adk.Message(nil), msgs...)
	baseAccumulatedCount := len(runAccumulatedMsgs)

	emptyHint := strings.TrimSpace(args.EmptyResponseMessage)
	if emptyHint == "" {
		emptyHint = "(Eino session completed but no assistant text was captured. Check process details or logs.) " +
			"（Eino 会话已完成，但未捕获到助手文本输出。请查看过程详情或日志。）"
	}

	lastAssistant = ""
	lastPlanExecuteExecutor = ""
	var reasoningStreamSeq int64
	var einoSubReplyStreamSeq int64
	toolEmitSeen := make(map[string]struct{})
	var einoMainRound int
	var einoLastAgent string
	subAgentToolStep := make(map[string]int)
	pendingByID := make(map[string]toolCallPendingInfo)
	pendingQueueByAgent := make(map[string][]string)
	markPending := func(tc toolCallPendingInfo) {
		if tc.ToolCallID == "" {
			return
		}
		pendingByID[tc.ToolCallID] = tc
		pendingQueueByAgent[tc.EinoAgent] = append(pendingQueueByAgent[tc.EinoAgent], tc.ToolCallID)
	}
	popNextPendingForAgent := func(agentName string) (toolCallPendingInfo, bool) {
		q := pendingQueueByAgent[agentName]
		for len(q) > 0 {
			id := q[0]
			q = q[1:]
			pendingQueueByAgent[agentName] = q
			if tc, ok := pendingByID[id]; ok {
				delete(pendingByID, id)
				return tc, true
			}
		}
		return toolCallPendingInfo{}, false
	}
	removePendingByID := func(toolCallID string) {
		if toolCallID == "" {
			return
		}
		delete(pendingByID, toolCallID)
	}
	flushAllPendingAsFailed := func(err error) {
		if progress == nil {
			pendingByID = make(map[string]toolCallPendingInfo)
			pendingQueueByAgent = make(map[string][]string)
			return
		}
		msg := ""
		if err != nil {
			msg = err.Error()
		}
		for _, tc := range pendingByID {
			toolName := tc.ToolName
			if strings.TrimSpace(toolName) == "" {
				toolName = "unknown"
			}
			progress("tool_result", fmt.Sprintf("工具结果 (%s)", toolName), map[string]interface{}{
				"toolName":       toolName,
				"success":        false,
				"isError":        true,
				"result":         msg,
				"resultPreview":  msg,
				"toolCallId":     tc.ToolCallID,
				"conversationId": conversationID,
				"einoAgent":      tc.EinoAgent,
				"einoRole":       tc.EinoRole,
				"source":         "eino",
			})
		}
		pendingByID = make(map[string]toolCallPendingInfo)
		pendingQueueByAgent = make(map[string][]string)
	}

	// 最近一次成功的 Eino filesystem execute 的标准输出（trim）：用于抑制模型紧接着复述同一字符串时的重复「助手输出」时间线。
	var executeStdoutDupMu sync.Mutex
	var pendingExecuteStdoutDup string
	recordPendingExecuteStdoutDup := func(toolName, stdout string, isErr bool) {
		if isErr || !strings.EqualFold(strings.TrimSpace(toolName), "execute") {
			return
		}
		t := strings.TrimSpace(stdout)
		if t == "" {
			return
		}
		executeStdoutDupMu.Lock()
		pendingExecuteStdoutDup = t
		executeStdoutDupMu.Unlock()
	}

	var toolResultSent sync.Map // toolCallID -> struct{}；与 ADK Tool 消息去重，避免 bridge 与事件流各推一次
	if args.ToolInvokeNotify != nil {
		args.ToolInvokeNotify.Set(func(toolCallID, toolName, einoAgent string, success bool, content string, invokeErr error) {
			tid := strings.TrimSpace(toolCallID)
			removePendingByID(tid)
			if tid == "" || progress == nil {
				return
			}
			if _, loaded := toolResultSent.LoadOrStore(tid, struct{}{}); loaded {
				return
			}
			isErr := !success || invokeErr != nil
			body := content
			if invokeErr != nil {
				body = invokeErr.Error()
				isErr = true
			}
			recordPendingExecuteStdoutDup(toolName, body, isErr)
			preview := body
			if len(preview) > 200 {
				preview = preview[:200] + "..."
			}
			agentTag := strings.TrimSpace(einoAgent)
			if agentTag == "" {
				agentTag = orchestratorName
			}
			progress("tool_result", fmt.Sprintf("工具结果 (%s)", toolName), map[string]interface{}{
				"toolName":       toolName,
				"success":        !isErr,
				"isError":        isErr,
				"result":         body,
				"resultPreview":  preview,
				"toolCallId":     tid,
				"conversationId": conversationID,
				"einoAgent":      agentTag,
				"einoRole":       einoRoleTag(agentTag),
				"source":         "eino",
			})
		})
	}

	runnerCfg := adk.RunnerConfig{
		Agent:           da,
		EnableStreaming: true,
	}
	var cpStore *fileCheckPointStore
	var checkPointID string
	if cp := strings.TrimSpace(args.CheckpointDir); cp != "" {
		cpDir := filepath.Join(cp, sanitizeEinoPathSegment(conversationID))
		st, stErr := newFileCheckPointStore(cpDir)
		if stErr != nil {
			if logger != nil {
				logger.Warn("eino checkpoint store disabled", zap.String("dir", cpDir), zap.Error(stErr))
			}
		} else {
			cpStore = st
			checkPointID = buildEinoCheckpointID(orchMode)
			runnerCfg.CheckPointStore = st
			if logger != nil {
				logger.Info("eino runner: checkpoint store enabled",
					zap.String("dir", cpDir),
					zap.String("checkPointID", checkPointID))
			}
		}
	}
	runner := adk.NewRunner(ctx, runnerCfg)
	var iter *adk.AsyncIterator[*adk.AgentEvent]
	if cpStore != nil && checkPointID != "" {
		if _, existed, getErr := cpStore.Get(ctx, checkPointID); getErr != nil {
			if logger != nil {
				logger.Warn("eino checkpoint preflight get failed", zap.String("checkPointID", checkPointID), zap.Error(getErr))
			}
		} else if existed {
			if progress != nil {
				progress("progress", "检测到断点，正在从中断节点恢复执行...", map[string]interface{}{
					"conversationId": conversationID,
					"source":         "eino",
					"orchestration":  orchMode,
					"checkPointID":   checkPointID,
				})
			}
			if logger != nil {
				logger.Info("eino runner: resume from checkpoint", zap.String("checkPointID", checkPointID))
			}
			resumeIter, resumeErr := runner.Resume(ctx, checkPointID)
			if resumeErr == nil {
				iter = resumeIter
			} else {
				if logger != nil {
					logger.Warn("eino runner: resume failed, fallback to fresh run",
						zap.String("checkPointID", checkPointID),
						zap.Error(resumeErr))
				}
				if progress != nil {
					progress("progress", "断点恢复失败，已回退为全新执行。", map[string]interface{}{
						"conversationId": conversationID,
						"source":         "eino",
						"orchestration":  orchMode,
						"checkPointID":   checkPointID,
					})
				}
			}
		}
	}
	if iter == nil {
		if checkPointID != "" {
			iter = runner.Run(ctx, msgs, adk.WithCheckPointID(checkPointID))
		} else {
			iter = runner.Run(ctx, msgs)
		}
	}
	handleRunErr := func(runErr error) error {
		if runErr == nil {
			return nil
		}
		if errors.Is(runErr, context.DeadlineExceeded) {
			flushAllPendingAsFailed(runErr)
			if progress != nil {
				progress("error", runErr.Error(), map[string]interface{}{
					"conversationId": conversationID,
					"source":         "eino",
					"errorKind":      "timeout",
				})
			}
			return runErr
		}
		// context.Canceled 是唯一应当直接终止编排的错误（用户关闭页面、主动停止等）。
		if errors.Is(runErr, context.Canceled) {
			flushAllPendingAsFailed(runErr)
			if progress != nil {
				progress("error", runErr.Error(), map[string]interface{}{
					"conversationId": conversationID,
					"source":         "eino",
				})
			}
			return runErr
		}
		if isEinoIterationLimitError(runErr) {
			flushAllPendingAsFailed(runErr)
			if progress != nil {
				progress("iteration_limit_reached", runErr.Error(), map[string]interface{}{
					"conversationId": conversationID,
					"source":         "eino",
					"orchestration":  orchMode,
				})
				progress("error", runErr.Error(), map[string]interface{}{
					"conversationId": conversationID,
					"source":         "eino",
					"errorKind":      "iteration_limit",
				})
			}
			return runErr
		}
		flushAllPendingAsFailed(runErr)
		if progress != nil {
			progress("error", runErr.Error(), map[string]interface{}{
				"conversationId": conversationID,
				"source":         "eino",
			})
		}
		return runErr
	}

	takePartial := func(runErr error) (*RunResult, error) {
		if len(runAccumulatedMsgs) <= baseAccumulatedCount {
			return nil, runErr
		}
		ids := snapshotMCPIDs()
		return buildEinoRunResultFromAccumulated(
			orchMode, runAccumulatedMsgs, lastAssistant, lastPlanExecuteExecutor, emptyHint, ids, true,
		), runErr
	}

	for {
		// 检测 context 取消（用户关闭浏览器、请求超时等），flush pending 工具状态避免 UI 卡在 "执行中"。
		select {
		case <-ctx.Done():
			flushAllPendingAsFailed(ctx.Err())
			if progress != nil {
				progress("error", "Request cancelled / 请求已取消", map[string]interface{}{
					"conversationId": conversationID,
					"source":         "eino",
				})
			}
			return takePartial(ctx.Err())
		default:
		}

		ev, ok := iter.Next()
		if !ok {
			// iter 结束并不总是“正常完成”：
			// 当取消/超时发生在 iter.Next() 阻塞期间时，可能直接返回 !ok。
			// 此时必须保留 checkpoint，避免后续恢复时被误判为“无断点”而全量重跑。
			if ctxErr := ctx.Err(); ctxErr != nil {
				flushAllPendingAsFailed(ctxErr)
				if progress != nil {
					progress("error", ctxErr.Error(), map[string]interface{}{
						"conversationId": conversationID,
						"source":         "eino",
					})
				}
				return takePartial(ctxErr)
			}
			if len(pendingByID) > 0 {
				orphanCount := len(pendingByID)
				flushAllPendingAsFailed(errors.New("pending tool call missing result before run completion"))
				if progress != nil {
					progress("eino_pending_orphaned", "pending tool calls were force-closed at run end", map[string]interface{}{
						"conversationId": conversationID,
						"source":         "eino",
						"orchestration":  orchMode,
						"pendingCount":   orphanCount,
					})
				}
			}
			if cpStore != nil && checkPointID != "" {
				if p, pErr := cpStore.path(checkPointID); pErr == nil {
					if rmErr := os.Remove(p); rmErr != nil && !os.IsNotExist(rmErr) && logger != nil {
						logger.Warn("eino checkpoint cleanup failed", zap.String("path", p), zap.Error(rmErr))
					}
				}
			}
			break
		}
		if ev == nil {
			continue
		}
		if ev.Err != nil {
			if retErr := handleRunErr(ev.Err); retErr != nil {
				return takePartial(retErr)
			}
		}
		if ev.AgentName != "" && progress != nil {
			iterEinoAgent := orchestratorName
			if orchMode == "plan_execute" {
				if a := strings.TrimSpace(ev.AgentName); a != "" {
					iterEinoAgent = a
				}
			}
			if streamsMainAssistant(ev.AgentName) {
				if einoMainRound == 0 {
					einoMainRound = 1
					progress("iteration", "", map[string]interface{}{
						"iteration":      1,
						"einoScope":      "main",
						"einoRole":       "orchestrator",
						"einoAgent":      iterEinoAgent,
						"orchestration":  orchMode,
						"conversationId": conversationID,
						"source":         "eino",
					})
				} else if einoLastAgent != "" && !streamsMainAssistant(einoLastAgent) {
					einoMainRound++
					progress("iteration", "", map[string]interface{}{
						"iteration":      einoMainRound,
						"einoScope":      "main",
						"einoRole":       "orchestrator",
						"einoAgent":      iterEinoAgent,
						"orchestration":  orchMode,
						"conversationId": conversationID,
						"source":         "eino",
					})
				}
			}
			einoLastAgent = ev.AgentName
			progress("progress", fmt.Sprintf("[Eino] %s", ev.AgentName), map[string]interface{}{
				"conversationId": conversationID,
				"einoAgent":      ev.AgentName,
				"einoRole":       einoRoleTag(ev.AgentName),
				"orchestration":  orchMode,
			})
		}
		if ev.Output == nil || ev.Output.MessageOutput == nil {
			continue
		}
		mv := ev.Output.MessageOutput

		if mv.IsStreaming && mv.MessageStream != nil {
			streamHeaderSent := false
			var reasoningStreamID string
			var toolStreamFragments []schema.ToolCall
			var subAssistantBuf string
			var subReplyStreamID string
			var mainAssistantBuf string
			var mainAssistDupTarget string // 非空表示本段主助手流需缓冲至 EOF，与 execute 输出比对去重
			var reasoningBuf string
			var streamRecvErr error
			for {
				chunk, rerr := mv.MessageStream.Recv()
				if rerr != nil {
					if errors.Is(rerr, io.EOF) {
						break
					}
					if logger != nil {
						logger.Warn("eino stream recv error, flushing incomplete stream",
							zap.Error(rerr),
							zap.String("agent", ev.AgentName),
							zap.Int("toolFragments", len(toolStreamFragments)))
					}
					streamRecvErr = rerr
					break
				}
				if chunk == nil {
					continue
				}
				if progress != nil && strings.TrimSpace(chunk.ReasoningContent) != "" {
					var reasoningDelta string
					reasoningBuf, reasoningDelta = normalizeStreamingDelta(reasoningBuf, chunk.ReasoningContent)
					if reasoningDelta != "" {
						if reasoningStreamID == "" {
							reasoningStreamID = fmt.Sprintf("eino-reasoning-%s-%d", conversationID, atomic.AddInt64(&reasoningStreamSeq, 1))
							progress("thinking_stream_start", " ", map[string]interface{}{
								"streamId":      reasoningStreamID,
								"source":        "eino",
								"einoAgent":     ev.AgentName,
								"einoRole":      einoRoleTag(ev.AgentName),
								"orchestration": orchMode,
							})
						}
						progress("thinking_stream_delta", reasoningDelta, map[string]interface{}{
							"streamId": reasoningStreamID,
						})
					}
				}
				if chunk.Content != "" {
					if progress != nil && streamsMainAssistant(ev.AgentName) {
						var contentDelta string
						mainAssistantBuf, contentDelta = normalizeStreamingDelta(mainAssistantBuf, chunk.Content)
						if contentDelta != "" {
							if mainAssistDupTarget == "" {
								executeStdoutDupMu.Lock()
								if pendingExecuteStdoutDup != "" {
									mainAssistDupTarget = pendingExecuteStdoutDup
								}
								executeStdoutDupMu.Unlock()
							}
							if mainAssistDupTarget != "" {
								// 已展示过 tool_result，缓冲全文；EOF 后与 execute 输出相同则不再发助手流
							} else {
								if !streamHeaderSent {
									progress("response_start", "", map[string]interface{}{
										"conversationId":     conversationID,
										"mcpExecutionIds":    snapshotMCPIDs(),
										"messageGeneratedBy": "eino:" + ev.AgentName,
										"einoRole":           "orchestrator",
										"einoAgent":          ev.AgentName,
										"orchestration":      orchMode,
									})
									streamHeaderSent = true
								}
								progress("response_delta", contentDelta, map[string]interface{}{
									"conversationId":  conversationID,
									"mcpExecutionIds": snapshotMCPIDs(),
									"einoRole":        "orchestrator",
									"einoAgent":       ev.AgentName,
									"orchestration":   orchMode,
								})
							}
						}
					} else if !streamsMainAssistant(ev.AgentName) {
						var subDelta string
						subAssistantBuf, subDelta = normalizeStreamingDelta(subAssistantBuf, chunk.Content)
						if subDelta != "" {
							if progress != nil {
								if subReplyStreamID == "" {
									subReplyStreamID = fmt.Sprintf("eino-sub-reply-%s-%d", conversationID, atomic.AddInt64(&einoSubReplyStreamSeq, 1))
									progress("eino_agent_reply_stream_start", "", map[string]interface{}{
										"streamId":       subReplyStreamID,
										"einoAgent":      ev.AgentName,
										"einoRole":       "sub",
										"conversationId": conversationID,
										"source":         "eino",
									})
								}
								progress("eino_agent_reply_stream_delta", subDelta, map[string]interface{}{
									"streamId":       subReplyStreamID,
									"conversationId": conversationID,
								})
							}
						}
					}
				}
				if len(chunk.ToolCalls) > 0 {
					toolStreamFragments = append(toolStreamFragments, chunk.ToolCalls...)
				}
			}
			if streamsMainAssistant(ev.AgentName) {
				s := strings.TrimSpace(mainAssistantBuf)
				if mainAssistDupTarget != "" {
					executeStdoutDupMu.Lock()
					pendingExecuteStdoutDup = ""
					executeStdoutDupMu.Unlock()
					if s != "" && s == mainAssistDupTarget {
						// 与刚展示的 execute 结果完全一致：不再发助手流式事件，仍写入轨迹与最终回复字段
						lastAssistant = s
						runAccumulatedMsgs = append(runAccumulatedMsgs, schema.AssistantMessage(s, nil))
						if orchMode == "plan_execute" && strings.EqualFold(strings.TrimSpace(ev.AgentName), "executor") {
							lastPlanExecuteExecutor = UnwrapPlanExecuteUserText(s)
						}
					} else if s != "" {
						if progress != nil {
							progress("response_start", "", map[string]interface{}{
								"conversationId":     conversationID,
								"mcpExecutionIds":    snapshotMCPIDs(),
								"messageGeneratedBy": "eino:" + ev.AgentName,
								"einoRole":           "orchestrator",
								"einoAgent":          ev.AgentName,
								"orchestration":      orchMode,
							})
							progress("response_delta", s, map[string]interface{}{
								"conversationId":  conversationID,
								"mcpExecutionIds": snapshotMCPIDs(),
								"einoRole":        "orchestrator",
								"einoAgent":       ev.AgentName,
								"orchestration":   orchMode,
							})
						}
						lastAssistant = s
						runAccumulatedMsgs = append(runAccumulatedMsgs, schema.AssistantMessage(s, nil))
						if orchMode == "plan_execute" && strings.EqualFold(strings.TrimSpace(ev.AgentName), "executor") {
							lastPlanExecuteExecutor = UnwrapPlanExecuteUserText(s)
						}
					}
				} else if s != "" {
					lastAssistant = s
					runAccumulatedMsgs = append(runAccumulatedMsgs, schema.AssistantMessage(s, nil))
					if orchMode == "plan_execute" && strings.EqualFold(strings.TrimSpace(ev.AgentName), "executor") {
						lastPlanExecuteExecutor = UnwrapPlanExecuteUserText(s)
					}
				}
			}
			if strings.TrimSpace(subAssistantBuf) != "" && progress != nil {
				if s := strings.TrimSpace(subAssistantBuf); s != "" {
					if subReplyStreamID != "" {
						progress("eino_agent_reply_stream_end", s, map[string]interface{}{
							"streamId":       subReplyStreamID,
							"einoAgent":      ev.AgentName,
							"einoRole":       "sub",
							"conversationId": conversationID,
							"source":         "eino",
						})
					} else {
						progress("eino_agent_reply", s, map[string]interface{}{
							"conversationId": conversationID,
							"einoAgent":      ev.AgentName,
							"einoRole":       "sub",
							"source":         "eino",
						})
					}
				}
			}
			var lastToolChunk *schema.Message
			if merged := mergeStreamingToolCallFragments(toolStreamFragments); len(merged) > 0 {
				lastToolChunk = &schema.Message{ToolCalls: merged}
			}
			tryEmitToolCallsOnce(lastToolChunk, ev.AgentName, orchestratorName, conversationID, progress, toolEmitSeen, subAgentToolStep, markPending)
			if streamRecvErr != nil {
				if progress != nil {
					progress("eino_stream_error", streamRecvErr.Error(), map[string]interface{}{
						"conversationId": conversationID,
						"source":         "eino",
						"einoAgent":      ev.AgentName,
						"einoRole":       einoRoleTag(ev.AgentName),
					})
				}
				if retErr := handleRunErr(streamRecvErr); retErr != nil {
					return takePartial(retErr)
				}
			}
			continue
		}

		msg, gerr := mv.GetMessage()
		if gerr != nil || msg == nil {
			continue
		}
		runAccumulatedMsgs = append(runAccumulatedMsgs, msg)
		tryEmitToolCallsOnce(mergeMessageToolCalls(msg), ev.AgentName, orchestratorName, conversationID, progress, toolEmitSeen, subAgentToolStep, markPending)

		if mv.Role == schema.Assistant {
			if progress != nil && strings.TrimSpace(msg.ReasoningContent) != "" {
				progress("thinking", strings.TrimSpace(msg.ReasoningContent), map[string]interface{}{
					"conversationId": conversationID,
					"source":         "eino",
					"einoAgent":      ev.AgentName,
					"einoRole":       einoRoleTag(ev.AgentName),
					"orchestration":  orchMode,
				})
			}
			body := strings.TrimSpace(msg.Content)
			if body != "" {
				if streamsMainAssistant(ev.AgentName) {
					executeStdoutDupMu.Lock()
					dup := pendingExecuteStdoutDup
					if dup != "" && body == dup {
						pendingExecuteStdoutDup = ""
						executeStdoutDupMu.Unlock()
						lastAssistant = body
						if orchMode == "plan_execute" && strings.EqualFold(strings.TrimSpace(ev.AgentName), "executor") {
							lastPlanExecuteExecutor = UnwrapPlanExecuteUserText(body)
						}
						// 非流式：与 execute 输出相同则跳过助手通道展示（msg 已在上方写入 runAccumulatedMsgs）
					} else {
						if dup != "" {
							pendingExecuteStdoutDup = ""
						}
						executeStdoutDupMu.Unlock()
						if progress != nil {
							progress("response_start", "", map[string]interface{}{
								"conversationId":     conversationID,
								"mcpExecutionIds":    snapshotMCPIDs(),
								"messageGeneratedBy": "eino:" + ev.AgentName,
								"einoRole":           "orchestrator",
								"einoAgent":          ev.AgentName,
								"orchestration":      orchMode,
							})
							progress("response_delta", body, map[string]interface{}{
								"conversationId":  conversationID,
								"mcpExecutionIds": snapshotMCPIDs(),
								"einoRole":        "orchestrator",
								"einoAgent":       ev.AgentName,
								"orchestration":   orchMode,
							})
						}
						lastAssistant = body
						if orchMode == "plan_execute" && strings.EqualFold(strings.TrimSpace(ev.AgentName), "executor") {
							lastPlanExecuteExecutor = UnwrapPlanExecuteUserText(body)
						}
					}
				} else if progress != nil {
					progress("eino_agent_reply", body, map[string]interface{}{
						"conversationId": conversationID,
						"einoAgent":      ev.AgentName,
						"einoRole":       "sub",
						"source":         "eino",
					})
				}
			}
		}

		if mv.Role == schema.Tool && progress != nil {
			toolName := msg.ToolName
			if toolName == "" {
				toolName = mv.ToolName
			}

			content := msg.Content
			isErr := false
			if strings.HasPrefix(content, einomcp.ToolErrorPrefix) {
				isErr = true
				content = strings.TrimPrefix(content, einomcp.ToolErrorPrefix)
			}

			preview := content
			if len(preview) > 200 {
				preview = preview[:200] + "..."
			}
			data := map[string]interface{}{
				"toolName":       toolName,
				"success":        !isErr,
				"isError":        isErr,
				"result":         content,
				"resultPreview":  preview,
				"conversationId": conversationID,
				"einoAgent":      ev.AgentName,
				"einoRole":       einoRoleTag(ev.AgentName),
				"source":         "eino",
			}
			toolCallID := strings.TrimSpace(msg.ToolCallID)
			if toolCallID == "" {
				if inferred, ok := popNextPendingForAgent(ev.AgentName); ok {
					toolCallID = inferred.ToolCallID
				} else if inferred, ok := popNextPendingForAgent(orchestratorName); ok {
					toolCallID = inferred.ToolCallID
				} else if inferred, ok := popNextPendingForAgent(""); ok {
					toolCallID = inferred.ToolCallID
				} else {
					for id := range pendingByID {
						toolCallID = id
						delete(pendingByID, id)
						break
					}
				}
			}
			if toolCallID != "" {
				removePendingByID(toolCallID)
				if _, loaded := toolResultSent.LoadOrStore(toolCallID, struct{}{}); loaded {
					continue
				}
				data["toolCallId"] = toolCallID
			}
			recordPendingExecuteStdoutDup(toolName, content, isErr)
			progress("tool_result", fmt.Sprintf("工具结果 (%s)", toolName), data)
		}
	}

	mcpIDsMu.Lock()
	ids := append([]string(nil), *mcpIDs...)
	mcpIDsMu.Unlock()

	out := buildEinoRunResultFromAccumulated(
		orchMode, runAccumulatedMsgs, lastAssistant, lastPlanExecuteExecutor, emptyHint, ids, false,
	)
	return out, nil
}

func einoPartialRunLastOutputHint() string {
	return "[执行未正常结束（用户停止、超时或异常）。续跑时请基于上文已产生的工具与结果继续，勿重复已完成步骤。]\n" +
		"[Run ended abnormally; continue from the trace above without repeating completed steps.]"
}

func buildEinoRunResultFromAccumulated(
	orchMode string,
	runAccumulatedMsgs []adk.Message,
	lastAssistant string,
	lastPlanExecuteExecutor string,
	emptyHint string,
	mcpIDs []string,
	partial bool,
) *RunResult {
	histJSON, _ := json.Marshal(runAccumulatedMsgs)
	cleaned := strings.TrimSpace(lastAssistant)
	if orchMode == "plan_execute" {
		if e := strings.TrimSpace(lastPlanExecuteExecutor); e != "" {
			cleaned = e
		} else {
			cleaned = UnwrapPlanExecuteUserText(cleaned)
		}
	}
	cleaned = dedupeRepeatedParagraphs(cleaned, 80)
	cleaned = dedupeParagraphsByLineFingerprint(cleaned, 100)
	// 防止超长响应导致 JSON 序列化慢或 OOM（多代理拼接大量工具输出时可能触发）。
	const maxResponseRunes = 100000
	if rs := []rune(cleaned); len(rs) > maxResponseRunes {
		cleaned = string(rs[:maxResponseRunes]) + "\n\n... (response truncated / 响应已截断)"
	}
	lastOut := cleaned
	resp := cleaned
	if partial && cleaned == "" {
		lastOut = einoPartialRunLastOutputHint()
		resp = emptyHint
	}
	out := &RunResult{
		Response:             resp,
		MCPExecutionIDs:      mcpIDs,
		LastAgentTraceInput:  string(histJSON),
		LastAgentTraceOutput: lastOut,
	}
	if !partial && out.Response == "" {
		out.Response = emptyHint
		out.LastAgentTraceOutput = out.Response
	}
	return out
}

func buildEinoCheckpointID(orchMode string) string {
	mode := sanitizeEinoPathSegment(strings.TrimSpace(orchMode))
	if mode == "" {
		mode = "default"
	}
	return "runner-" + mode
}
