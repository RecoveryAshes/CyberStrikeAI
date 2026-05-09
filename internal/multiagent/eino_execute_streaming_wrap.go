package multiagent

import (
	"context"
	"errors"
	"fmt"
	"io"
	"strings"

	"cyberstrike-ai/internal/einomcp"
	"cyberstrike-ai/internal/security"

	"github.com/cloudwego/eino/adk/filesystem"
	"github.com/cloudwego/eino/compose"
	"github.com/cloudwego/eino/schema"
)

// einoStreamingShellWrap 包装 Eino filesystem 使用的 StreamingShell（cloudwego eino-ext local.Local）。
// 官方 execute 工具默认走 ExecuteStreaming 且不设 RunInBackendGround；末尾带 & 时子进程仍与管道相连，
// streamStdout 按行读取会在无换行输出时长时间阻塞（与 MCP 工具 exec 的独立实现不同）。
// 对「完全后台」命令自动开启 RunInBackendGround，与 local.runCmdInBackground 行为对齐。
//
// 使用 Pipe 将内层流转发给调用方：在 inner EOF 后、关闭 Pipe 前同步调用 ToolInvokeNotify.Fire，
// 保证 run loop 在模型开始下一轮输出前已记录 execute 结果（用于 UI 与「重复助手复述」去重）。
type einoStreamingShellWrap struct {
	inner         filesystem.StreamingShell
	invokeNotify  *einomcp.ToolInvokeNotifyHolder
	einoAgentName string
	// recordMonitor 在 execute 流结束后写入 tool_executions 并 recorder(executionId)，使「渗透测试详情」与常规 MCP 一致。
	recordMonitor func(command, stdout string, success bool, invokeErr error)
}

func (w *einoStreamingShellWrap) ExecuteStreaming(ctx context.Context, input *filesystem.ExecuteRequest) (*schema.StreamReader[*filesystem.ExecuteResponse], error) {
	if w.inner == nil {
		return nil, fmt.Errorf("einoStreamingShellWrap: inner shell is nil")
	}
	if input == nil {
		return w.inner.ExecuteStreaming(ctx, nil)
	}
	req := *input
	cmd := strings.TrimSpace(req.Command)
	if security.IsBackgroundShellCommand(req.Command) && !req.RunInBackendGround {
		req.RunInBackendGround = true
	}
	sr, err := w.inner.ExecuteStreaming(ctx, &req)
	if err != nil {
		return nil, err
	}
	tid := strings.TrimSpace(compose.GetToolCallID(ctx))
	if sr == nil || w.invokeNotify == nil || tid == "" {
		return sr, nil
	}

	outR, outW := schema.Pipe[*filesystem.ExecuteResponse](32)
	agentTag := strings.TrimSpace(w.einoAgentName)

	go func(inner *schema.StreamReader[*filesystem.ExecuteResponse], command string) {
		defer inner.Close()

		var sb strings.Builder
		const maxCapture = 16 * 1024
		success := true
		var invokeErr error
		exitCode := 0
		hasExitCode := false

		for {
			resp, rerr := inner.Recv()
			if errors.Is(rerr, io.EOF) {
				break
			}
			if rerr != nil {
				success = false
				invokeErr = rerr
				_ = outW.Send(nil, rerr)
				break
			}
			if resp != nil {
				if resp.ExitCode != nil {
					hasExitCode = true
					exitCode = *resp.ExitCode
				}
				if remain := maxCapture - sb.Len(); remain > 0 {
					out := resp.Output
					if len(out) > remain {
						out = out[:remain]
					}
					sb.WriteString(out)
				}
				if outW.Send(resp, nil) {
					success = false
					invokeErr = fmt.Errorf("execute stream closed by consumer")
					break
				}
			}
		}

		if success && hasExitCode && exitCode != 0 {
			success = false
			invokeErr = fmt.Errorf("execute exited with code %d", exitCode)
		}
		if w.recordMonitor != nil {
			w.recordMonitor(command, sb.String(), success, invokeErr)
		}
		w.invokeNotify.Fire(tid, "execute", agentTag, success, sb.String(), invokeErr)
		outW.Close()
	}(sr, cmd)

	return outR, nil
}
