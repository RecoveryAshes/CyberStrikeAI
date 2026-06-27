import type { SSEEnvelope } from "../api/types";
import { compactText } from "../lib/utils";
import { normalizePlanItems } from "./planParser";
import type { RunEvent, RuntimeAction, ToolRun } from "./types";

function eventId() {
  return `evt-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

function runtimeTraceOf(envelope: SSEEnvelope): Record<string, unknown> | null {
  const trace = envelope.data?.runtimeTrace;
  if (trace && typeof trace === "object") return trace as Record<string, unknown>;
  return null;
}

function makeEvent(type: string, label: string, detail?: string, raw?: unknown): RunEvent {
  return {
    id: eventId(),
    type,
    label,
    detail,
    time: new Date().toISOString(),
    raw
  };
}

function makeProgressUpdate(message: string, trace?: Record<string, unknown> | null) {
  return {
    id: firstText(trace?.id, trace?.eventId) || eventId(),
    message,
    time: new Date().toISOString(),
    turnId: firstText(trace?.turnId, trace?.turn_id),
    assistantMessageId: firstText(trace?.assistantMessageId, trace?.assistant_message_id)
  };
}

function firstText(...values: unknown[]) {
  for (const value of values) {
    if (typeof value === "string" || typeof value === "number") {
      const text = String(value).trim();
      if (text && text !== "[object Object]" && text !== "<nil>") return text;
    }
  }
  return "";
}

function toolFromTrace(trace: Record<string, unknown>, status: ToolRun["status"]): ToolRun {
  const nestedTool = trace.tool && typeof trace.tool === "object" ? (trace.tool as Record<string, unknown>) : {};
  const name =
    firstText(
      trace.toolName,
      trace.tool_name,
      trace.name,
      nestedTool.identity,
      nestedTool.name,
      nestedTool.toolName,
      nestedTool.tool_name,
      nestedTool.mcpName,
      trace.tool,
      trace.functionName,
      "tool"
    ) || "tool";
  const id =
    firstText(
      trace.toolCallId,
      trace.toolCallID,
      trace.tool_call_id,
      nestedTool.callId,
      nestedTool.toolCallID,
      nestedTool.toolCallId,
      trace.requestID,
      trace.id,
      name,
      eventId()
    ) || eventId();
  return {
    id,
    name,
    status,
    input: trace.input || trace.argumentsObj || trace.arguments || trace.args || nestedTool.arguments,
    output: compactText(trace.result || trace.error || trace.delta || nestedTool.result || nestedTool.error, ""),
    startedAt: status === "running" ? new Date().toISOString() : undefined,
    completedAt: status !== "running" ? new Date().toISOString() : undefined
  };
}

function toolIdFromTrace(trace?: Record<string, unknown> | null) {
  if (!trace) return "";
  const nestedTool = trace.tool && typeof trace.tool === "object" ? (trace.tool as Record<string, unknown>) : {};
  return firstText(
    trace.toolCallId,
    trace.toolCallID,
    trace.tool_call_id,
    nestedTool.callId,
    nestedTool.toolCallId,
    nestedTool.toolCallID,
    trace.requestID,
    trace.id
  );
}

export function parseSSELine(line: string): SSEEnvelope | null {
  const trimmed = line.trim();
  if (!trimmed || trimmed.startsWith(":")) return null;
  const data = trimmed.startsWith("data:") ? trimmed.slice(5).trim() : trimmed;
  if (!data || data === "[DONE]") return { type: "done" };
  try {
    return JSON.parse(data) as SSEEnvelope;
  } catch {
    return { type: "progress", message: data };
  }
}

function assistantMessageIdFromEnvelope(envelope: SSEEnvelope, trace?: Record<string, unknown> | null) {
  return firstText(
    envelope.data?.assistantMessageId,
    envelope.data?.assistant_message_id,
    trace?.assistantMessageId,
    trace?.assistant_message_id
  );
}

export function adaptSSE(envelope: SSEEnvelope): RuntimeAction[] {
  const actions: RuntimeAction[] = [];
  const outerType = String(envelope.type || envelope.event || "progress");
  const trace = runtimeTraceOf(envelope);
  const runtimeType = compactText(envelope.data?.runtimeEventType || trace?.event || trace?.type, "");
  const type = trace || runtimeType ? String(runtimeType || outerType) : outerType;
  const message = compactText(envelope.message || trace?.message || "");
  const assistantMessageId = assistantMessageIdFromEnvelope(envelope, trace);
  const event = makeEvent(type, message || type, compactText(trace || envelope.data, ""), envelope);
  actions.push({ type: "event", event, patch: assistantMessageId ? { assistantMessageId } : undefined });

  switch (type) {
    case "conversation": {
      const cid = compactText(envelope.data?.conversationId || envelope.data?.id);
      if (cid) actions.push({ type: "event", event, patch: { conversationId: cid } });
      break;
    }
    case "message_saved":
      break;
    case "assistant_delta":
    case "response_delta":
      actions.push({
        type: "assistant_delta",
        delta: compactText(trace?.delta || envelope.message || envelope.data?.delta),
        accumulated: compactText(trace?.accumulated || envelope.data?.accumulated, "") || undefined
      });
      break;
    case "assistant_progress_update": {
      const progressMessage = compactText(trace?.message || envelope.message || envelope.data?.message);
      const progressTrace = { ...(envelope.data || {}), ...(trace || {}) };
      if (progressMessage) actions.push({ type: "progress_update", update: makeProgressUpdate(progressMessage, progressTrace) });
      break;
    }
    case "runtime_status_update":
      break;
    case "task_updated":
    case "task_completed":
    case "task_removed":
      break;
    case "hitl_pending_updated":
    case "hitl_decision_updated":
      break;
    case "reasoning_delta":
    case "reasoning_chain_stream_delta":
    case "thinking_stream_delta":
      actions.push({
        type: "reasoning_delta",
        delta: compactText(trace?.delta || envelope.message || envelope.data?.delta),
        accumulated: compactText(trace?.accumulated || envelope.data?.accumulated, "") || undefined
      });
      break;
    case "plan_updated":
    case "planning":
      actions.push({
        type: "plan",
        items: normalizePlanItems(trace?.items || trace?.plan || envelope.data?.items || envelope.data?.plan || envelope.message)
      });
      break;
    case "todo_updated":
      actions.push({
        type: "plan",
        conversationId: firstText(envelope.data?.conversationId, trace?.conversationId),
        items: normalizePlanItems(envelope.data?.todos || envelope.data?.items || trace?.todos || trace?.items, "todo")
      });
      break;
    case "todo_cleared":
      actions.push({
        type: "plan",
        conversationId: firstText(envelope.data?.conversationId, trace?.conversationId),
        items: []
      });
      break;
    case "tool_call_started":
    case "tool_call":
      actions.push({ type: "tool", tool: toolFromTrace(trace || (envelope.data || {}), "running") });
      break;
    case "tool_call_delta":
    case "tool_result_delta": {
      const data = trace || envelope.data || {};
      const toolId = toolIdFromTrace(data);
      if (toolId) actions.push({ type: "tool_delta", toolId, delta: compactText(data.delta || envelope.message || data.result) });
      break;
    }
    case "tool_call_completed":
    case "tool_result":
      actions.push({ type: "tool", tool: toolFromTrace(trace || (envelope.data || {}), "completed") });
      break;
    case "tool_call_failed":
      actions.push({ type: "tool", tool: toolFromTrace(trace || (envelope.data || {}), "failed") });
      break;
    case "approval_requested":
    case "hitl_approval_requested": {
      const data = envelope.data || {};
      const id = compactText(trace?.requestID || data.interruptId || data.requestId || data.id);
      actions.push({
        type: "approval",
        approval: {
          id,
          conversationId: compactText(data.conversationId || trace?.conversationID),
          toolName: compactText(trace?.toolName || data.toolName),
          toolCallId: compactText(trace?.toolCallID || data.toolCallId),
          payload: compactText(trace || data),
          status: "pending",
          createdAt: new Date().toISOString()
        }
      });
      break;
    }
    case "approval_resolved":
    case "hitl_approval_resolved":
      actions.push({ type: "event", event, patch: { status: "running" } });
      break;
    case "compaction_started":
    case "compaction_completed":
      actions.push({ type: "event", event, patch: { status: "running" } });
      break;
    case "turn_completed":
    case "done":
    case "response":
      {
        const finalResponse = compactText(trace?.response || envelope.data?.response || envelope.message, "");
        if (finalResponse) {
          actions.push({ type: "assistant_delta", delta: "", accumulated: finalResponse });
        }
      }
      actions.push({ type: "finish", status: "completed" });
      break;
    case "turn_aborted":
    case "cancelled":
      actions.push({ type: "finish", status: "cancelled" });
      break;
    case "runtime_error":
    case "error":
      actions.push({ type: "finish", status: "error", error: message || "Runtime error" });
      break;
    default:
      break;
  }
  return actions;
}

export function processDetailsToEvents(details: { eventType: string; message?: string; data?: unknown; createdAt?: string; id?: string }[]) {
  return [...details].reverse().map((detail) => ({
    id: detail.id || eventId(),
    type: detail.eventType,
    label: detail.message || detail.eventType,
    detail: compactText(detail.data, ""),
    time: detail.createdAt || new Date().toISOString(),
    raw: detail
  }));
}
