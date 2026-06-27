import { Fragment, useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  Bot,
  Brain,
  Check,
  ChevronLeft,
  ChevronDown,
  ChevronRight,
  CircleStop,
  Code2,
  FilePenLine,
  FolderKanban,
  GitBranch,
  MessageSquare,
  MoreHorizontal,
  Plus,
  Search,
  Send,
  Settings,
  ShieldAlert,
  ShieldCheck,
  Sparkles,
  Trash2,
  UserRound
} from "lucide-react";
import DOMPurify from "dompurify";
import { marked } from "marked";
import { Api } from "../../api/resources";
import type { AgentTask, AppConfig, ChatRequest, ConversationMessage, HITLConfig, HITLPendingItem, ProcessDetail, RuntimeTodoItem, SSEEnvelope } from "../../api/types";
import { ApiError, apiStream, getAuthToken } from "../../api/client";
import { adaptSSE, parseSSELine, processDetailsToEvents } from "../../runtime/eventAdapter";
import { initialRuntimeState, runtimeReducer } from "../../runtime/reducer";
import {
  progressPreview,
  runActivityItems,
  todoProgress,
  type RunActivityItem,
  type RuntimeCell
} from "../../runtime/transcriptActivity";
import { activeRunAssistantMessageId } from "../../runtime/transcriptLayout";
import { normalizePlanItems } from "../../runtime/planParser";
import type { PlanItem, ProgressUpdate, RunEvent, RuntimeAction, ToolRun, TurnRun } from "../../runtime/types";
import { cn, compactText, formatTime } from "../../lib/utils";
import { useWorkbenchData } from "./useWorkbenchData";
import { Button } from "../ui/button";
import { Input } from "../ui/input";
import { Textarea } from "../ui/textarea";
import { Badge } from "../ui/badge";
import { ScrollArea } from "../ui/scroll-area";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "../ui/select";
import { Sheet, SheetContent, SheetHeader, SheetTitle, SheetTrigger } from "../ui/sheet";
import { Popover, PopoverContent, PopoverTrigger } from "../ui/popover";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../ui/tabs";
import { Tooltip, TooltipContent, TooltipTrigger } from "../ui/tooltip";
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from "../ui/command";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle
} from "../ui/dialog";

const navItems = [
  { id: "chat", label: "Chat", icon: MessageSquare },
  { id: "projects", label: "Projects", icon: FolderKanban },
  { id: "hitl", label: "HITL", icon: ShieldCheck },
  { id: "mcp", label: "MCP", icon: GitBranch },
  { id: "knowledge", label: "Knowledge", icon: Brain },
  { id: "skills", label: "Skills", icon: Sparkles },
  { id: "agents", label: "Agents", icon: Bot },
  { id: "settings", label: "Settings", icon: Settings }
];

const reasoningEfforts = ["low", "medium", "high", "max", "xhigh"];
const defaultReasoningEffort = "xhigh";
const hitlChoices = [
  {
    mode: "off",
    label: "完全访问",
    description: "不拦截工具调用",
    enabled: false,
    tone: "orange"
  },
  {
    mode: "approval",
    label: "人工审批",
    description: "敏感工具调用前请求确认",
    enabled: true,
    tone: "blue"
  },
  {
    mode: "auto",
    label: "自动判断",
    description: "按策略判断是否需要确认",
    enabled: true,
    tone: "violet"
  }
] as const;
const EMPTY_CONVERSATIONS: never[] = [];
const EMPTY_MESSAGES: never[] = [];
const EMPTY_ROLES: never[] = [];
const EMPTY_PROJECTS: never[] = [];
const EMPTY_TASKS: never[] = [];
const EMPTY_RUNTIME_TODOS: RuntimeTodoItem[] = [];
const RECENT_COMPLETED_RUN_REUSE_MS = 60_000;

function updateOpenAIModel(config: AppConfig | undefined, model: string): AppConfig {
  return {
    ...(config || {}),
    openai: {
      ...(config?.openai || {}),
      model
    }
  };
}

function normalizeReasoningEffort(effort?: string) {
  return reasoningEfforts.includes(effort || "") ? effort || defaultReasoningEffort : defaultReasoningEffort;
}

function normalizeModelOptions(models: unknown) {
  if (!Array.isArray(models)) return [];
  return [
    ...new Set(
      models
        .map((model) => {
          if (typeof model === "string") return model;
          if (model && typeof model === "object") {
            const item = model as { id?: unknown; name?: unknown; model?: unknown };
            return firstNonObjectText(item.id, item.name, item.model);
          }
          return "";
        })
        .map((model) => model.trim())
        .filter(Boolean)
    )
  ];
}

function hitlChoiceFor(hitl: HITLConfig) {
  const mode = hitl.enabled ? hitl.mode || "approval" : "off";
  if (mode === "on") return hitlChoices[1];
  return hitlChoices.find((choice) => choice.mode === mode) || hitlChoices[0];
}

function markdownHtml(content: string) {
  return DOMPurify.sanitize(marked.parse(content || "", { async: false }) as string);
}

function firstNonObjectText(...values: unknown[]) {
  for (const value of values) {
    if (typeof value === "string" || typeof value === "number") {
      const text = String(value).trim();
      if (text && text !== "[object Object]" && text !== "<nil>") return text;
    }
  }
  return "";
}

function eventTimeBounds(events: RunEvent[]) {
  const times = events
    .map((event) => (event.time ? new Date(event.time).getTime() : Number.NaN))
    .filter((time) => !Number.isNaN(time))
    .sort((a, b) => a - b);
  if (times.length === 0) return {};
  return {
    startedAt: new Date(times[0]).toISOString(),
    completedAt: new Date(times[times.length - 1]).toISOString()
  };
}

function mergeRunActivity(primary: TurnRun, fallback?: TurnRun): TurnRun {
  if (!fallback) return primary;
  const progressById = new Map<string, ProgressUpdate>();
  for (const update of [...primary.progressUpdates, ...fallback.progressUpdates]) {
    progressById.set(update.id, update);
  }
  const eventsById = new Map<string, RunEvent>();
  for (const event of [...primary.events, ...fallback.events]) {
    eventsById.set(event.id, event);
  }
  const fallbackIsLive = fallback.status === "running" || fallback.status === "awaiting_approval";
  const primaryIsRestoredCompleted = primary.id.startsWith("restored-") && primary.status === "completed";
  return {
    ...primary,
    assistantMessageId: primary.assistantMessageId || fallback.assistantMessageId,
    assistantText: primary.assistantText || fallback.assistantText,
    reasoningText: primary.reasoningText || fallback.reasoningText,
    progressUpdates: [...progressById.values()],
    status: primaryIsRestoredCompleted ? primary.status : fallbackIsLive ? fallback.status : primary.status,
    error: primary.error || fallback.error,
    plan: primary.plan.length ? primary.plan : fallback.plan,
    tools: { ...fallback.tools, ...primary.tools },
    approvals: primary.approvals.length ? primary.approvals : fallback.approvals,
    events: [...eventsById.values()],
    startedAt: primary.startedAt || fallback.startedAt,
    completedAt: primaryIsRestoredCompleted
      ? primary.completedAt || fallback.completedAt
      : fallbackIsLive
        ? undefined
        : primary.completedAt || fallback.completedAt
  };
}

function runElapsedLabel(run: TurnRun) {
  const startedAt = run.startedAt ? new Date(run.startedAt).getTime() : 0;
  const endedAt = run.completedAt ? new Date(run.completedAt).getTime() : Date.now();
  if (!startedAt || Number.isNaN(startedAt) || Number.isNaN(endedAt) || endedAt < startedAt) return "";
  const totalSeconds = Math.max(1, Math.round((endedAt - startedAt) / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  if (minutes > 0) return `${minutes}m ${seconds}s`;
  return `${seconds}s`;
}

function runStatusLabel(run: TurnRun) {
  if (run.status === "running") return "处理中";
  if (run.status === "awaiting_approval") return "等待审批";
  if (run.status === "cancelled") return "已取消";
  if (run.status === "error") return "处理失败";
  return "已处理";
}

function groupConversations(items: { updatedAt?: string }[]) {
  const now = new Date();
  return items.map((item) => {
    const updated = item.updatedAt ? new Date(item.updatedAt) : now;
    const diff = now.getTime() - updated.getTime();
    if (diff < 1000 * 60 * 60 * 24) return "Today";
    if (diff < 1000 * 60 * 60 * 24 * 7) return "This Week";
    return "Earlier";
  });
}

function isAssistantPlaceholder(content?: string) {
  const text = (content || "").trim();
  return text === "处理中..." || text.toLowerCase() === "processing...";
}

function conversationIdFromEnvelope(envelope: SSEEnvelope) {
  const data = envelope.data || {};
  const trace = data.runtimeTrace && typeof data.runtimeTrace === "object" ? data.runtimeTrace as Record<string, unknown> : {};
  return firstNonObjectText(data.conversationId, data.conversation_id, trace.conversationId, trace.conversation_id);
}

function envelopeType(envelope: SSEEnvelope) {
  const data = envelope.data || {};
  const trace = data.runtimeTrace && typeof data.runtimeTrace === "object" ? data.runtimeTrace as Record<string, unknown> : {};
  return firstNonObjectText(data.runtimeEventType, trace.event, trace.type, envelope.type, envelope.event);
}

function isAcceptedRunEnvelope(envelope: SSEEnvelope) {
  const type = envelopeType(envelope);
  return (
    type === "session_started" ||
    type === "turn_started" ||
    type === "runtime_status_update" ||
    type === "plan_updated" ||
    type === "assistant_progress_update" ||
    type === "reasoning_delta" ||
    type === "reasoning_chain_stream_delta" ||
    type === "tool_call_started" ||
    type === "tool_call_delta" ||
    type === "tool_call" ||
    type === "tool_result_delta" ||
    type === "tool_call_completed" ||
    type === "tool_call_failed" ||
    type === "tool_result" ||
    type === "response_delta" ||
    type === "assistant_delta"
  );
}

function isTerminalRunEnvelope(envelope: SSEEnvelope) {
  const type = envelopeType(envelope);
  return type === "done" || type === "response" || type === "turn_completed" || type === "turn_aborted" || type === "cancelled" || type === "error" || type === "runtime_error";
}

function runtimeTodoItemsFromUnknown(raw: unknown): RuntimeTodoItem[] {
  return Array.isArray(raw) ? raw.filter((item): item is RuntimeTodoItem => Boolean(item && typeof item === "object")) : [];
}

function planItemsFromRuntimeTodos(todos: RuntimeTodoItem[]): PlanItem[] {
  return todos
    .slice()
    .sort((a, b) => (Number(a.position ?? 0) - Number(b.position ?? 0)))
    .map((todo, index) => ({
      id: String(todo.itemId || todo.item_id || todo.id || `todo-${index}`),
      content: String(todo.content || `Step ${index + 1}`),
      status: normalizePlanItems([{ status: todo.status, content: todo.content, id: todo.itemId || todo.item_id || todo.id }], "todo")[0]?.status || "pending"
    }));
}

function taskFromEnvelope(envelope: SSEEnvelope): AgentTask | null {
  const data = envelope.data || {};
  const task = data.task && typeof data.task === "object" ? (data.task as AgentTask) : null;
  const conversationId = firstNonObjectText(task?.conversationId, data.conversationId);
  if (!conversationId) return null;
  return { ...(task || {}), conversationId };
}

function taskIsActive(task: AgentTask) {
  const active = task.active;
  if (typeof active === "boolean") return active;
  const status = String(task.status || "").toLowerCase();
  return !["completed", "cancelled", "canceled", "failed", "error"].includes(status);
}

function runningRunForConversation(runs: TurnRun[], conversationId?: string) {
  if (!conversationId) return null;
  return runs.find(
    (run) => run.conversationId === conversationId && (run.status === "running" || run.status === "awaiting_approval")
  ) || null;
}

export function recentCompletedRunForConversation(runs: TurnRun[], conversationId?: string) {
  if (!conversationId) return null;
  const run = runs.find((item) => item.conversationId === conversationId && item.status === "completed" && item.completedAt);
  if (!run?.completedAt) return null;
  const completedAt = Date.parse(run.completedAt);
  if (Number.isNaN(completedAt)) return null;
  const age = Date.now() - completedAt;
  return age >= 0 && age <= RECENT_COMPLETED_RUN_REUSE_MS ? run : null;
}

export function displayRunForConversation(runs: TurnRun[], conversationId?: string) {
  if (!conversationId) return null;
  return runningRunForConversation(runs, conversationId) ||
    runs.find((run) => run.conversationId === conversationId && run.assistantText.trim()) ||
    runs.find((run) => run.conversationId === conversationId) ||
    null;
}

function taskStartedAt(task: AgentTask) {
  return typeof task.startedAt === "string" ? task.startedAt : undefined;
}

function parseJSON(value: string): { ok: true; value: unknown } | { ok: false } {
  const trimmed = value.trim();
  if (!trimmed || (!trimmed.startsWith("{") && !trimmed.startsWith("["))) return { ok: false };
  try {
    return { ok: true, value: JSON.parse(trimmed) };
  } catch {
    return { ok: false };
  }
}

function readableToolOutput(value: unknown): string {
  if (typeof value === "string") {
    const parsed = parseJSON(value);
    if (parsed.ok) return readableToolOutput(parsed.value) || value;
    return value;
  }
  if (!value || typeof value !== "object" || Array.isArray(value)) return compactText(value, "");
  const obj = value as Record<string, unknown>;
  const result = typeof obj.result === "string" ? obj.result : "";
  if (result) {
    const nested = readableToolOutput(result);
    if (nested) return nested;
  }
  const stdout = typeof obj.stdout === "string" ? obj.stdout : "";
  const stderr = typeof obj.stderr === "string" ? obj.stderr : "";
  if (stdout || stderr) return [stdout, stderr ? `stderr:\n${stderr}` : ""].filter(Boolean).join("\n");
  return firstNonObjectText(obj.output, obj.text, obj.content, obj.message, result);
}

function restoredRunFromDetails(
  conversationId: string | undefined,
  details: ProcessDetail[],
  events: RunEvent[],
  assistantMessage?: ConversationMessage
): TurnRun | null {
  if (!conversationId || (details.length === 0 && !assistantMessage?.reasoningContent)) return null;
  const planMap = new Map<string, PlanItem>();
  const tools: Record<string, ToolRun> = {};
  const progressUpdates: ProgressUpdate[] = [];
  let reasoningText = assistantMessage?.reasoningContent || "";

  for (const detail of details) {
    if (!detail || typeof detail !== "object") continue;
    const obj = detail as Record<string, unknown>;
    const data = obj.data && typeof obj.data === "object" ? (obj.data as Record<string, unknown>) : {};
    const trace = data.runtimeTrace && typeof data.runtimeTrace === "object" ? (data.runtimeTrace as Record<string, unknown>) : {};
    const runtimeType = String(data.runtimeEventType || trace.event || trace.type || "");
    const eventType = String(obj.eventType || runtimeType || "");

    const planItems = normalizePlanItems(data.items || data.plan || trace.items || trace.plan || obj.message, String(obj.id || "plan"));
    if ((eventType === "planning" || runtimeType === "plan_updated") && planItems.length > 0) {
      planItems.forEach((item) => {
        planMap.set(item.id, item);
      });
    }

    const toolPayload = trace.tool && typeof trace.tool === "object" ? (trace.tool as Record<string, unknown>) : data;
    const argumentsObj = data.argumentsObj || toolPayload.arguments;
    const argumentTool =
      argumentsObj && typeof argumentsObj === "object"
        ? String((argumentsObj as Record<string, unknown>).tool || (argumentsObj as Record<string, unknown>).name || "")
        : "";
    const toolName = firstNonObjectText(
      toolPayload.identity,
      data.toolName,
      toolPayload.mcpName,
      toolPayload.name,
      toolPayload.toolName,
      toolPayload.tool_name,
      argumentTool
    );
    const toolId = firstNonObjectText(data.toolCallId, toolPayload.callId, toolPayload.toolCallID, toolPayload.toolCallId, toolName, obj.id);
    if ((eventType === "tool_call" || eventType === "tool_result" || eventType === "tool_result_delta" || runtimeType.startsWith("tool_call")) && toolId) {
      const failed = (eventType === "tool_result" && data.success === false) || runtimeType === "tool_call_failed";
      const completed = eventType === "tool_result" || runtimeType === "tool_call_completed" || runtimeType === "tool_call_failed";
      const isDelta = eventType === "tool_result_delta" || runtimeType === "tool_call_delta";
      const previousOutput = tools[toolId]?.output || "";
      const deltaOutput = compactText(data.delta || trace.delta || obj.message || "", "");
      const completedOutput = readableToolOutput(data.result || toolPayload.result || data.error || toolPayload.error || obj.message || "");
      tools[toolId] = {
        ...tools[toolId],
        id: toolId,
        name: toolName || tools[toolId]?.name || "tool",
        status: failed ? "failed" : completed ? "completed" : "running",
        input: tools[toolId]?.input || argumentsObj,
        output: isDelta ? `${previousOutput}${deltaOutput}` : completedOutput || previousOutput,
        startedAt: tools[toolId]?.startedAt || String(obj.createdAt || ""),
        completedAt: completed ? String(obj.createdAt || "") : tools[toolId]?.completedAt
      };
    }
    if (
      eventType === "assistant_progress_update" ||
      runtimeType === "assistant_progress_update"
    ) {
      const message = compactText(obj.message || data.message || trace.message, "");
      if (message) {
        progressUpdates.push({
          id: String(obj.id || `${assistantMessage?.id || conversationId}-progress-${progressUpdates.length}`),
          message,
          time: String(obj.createdAt || new Date().toISOString()),
          turnId: compactText(data.turnId || trace.turnId, "") || undefined,
          assistantMessageId: compactText(data.assistantMessageId || trace.assistantMessageId, "") || assistantMessage?.id
        });
      }
    }
    if (
      eventType === "reasoning_chain" ||
      eventType === "reasoning_chain_stream_delta" ||
      eventType === "thinking" ||
      eventType === "thinking_stream_delta" ||
      runtimeType === "reasoning_delta"
    ) {
      const accumulated = compactText(data.accumulated || data.__sse_accumulated || trace.accumulated, "");
      const delta = compactText(obj.message || data.delta || trace.delta, "");
      if (accumulated) {
        reasoningText = accumulated;
      } else if (delta) {
        reasoningText = reasoningText ? `${reasoningText}${delta}` : delta;
      }
    }
  }

  if (planMap.size === 0 && Object.keys(tools).length === 0 && progressUpdates.length === 0 && !reasoningText.trim()) return null;
  const bounds = eventTimeBounds(events);
  return {
    id: `restored-${assistantMessage?.id || conversationId}`,
    conversationId,
    assistantMessageId: assistantMessage?.id,
    assistantText: "",
    reasoningText,
    progressUpdates,
    status: "completed",
    plan: [...planMap.values()],
    tools,
    approvals: [],
    events,
    startedAt: bounds.startedAt,
    completedAt: bounds.completedAt
  };
}

export function ChatWorkbench() {
  const queryClient = useQueryClient();
  const [search, setSearch] = useState("");
  const [conversationId, setConversationId] = useState<string | undefined>();
  const [input, setInput] = useState("");
  const [selectedRole, setSelectedRole] = useState("");
  const [selectedProject, setSelectedProject] = useState("");
  const [selectedModel, setSelectedModel] = useState("");
  const [modelOptions, setModelOptions] = useState<string[]>([]);
  const [modelError, setModelError] = useState("");
  const [reasoningEffort, setReasoningEffort] = useState(defaultReasoningEffort);
  const [hitlConfig, setHitlConfig] = useState<HITLConfig>({ enabled: false, mode: "off", sensitiveTools: [], timeoutSeconds: 600 });
  const [traceOpen, setTraceOpen] = useState(false);
  const [hoveredRun, setHoveredRun] = useState<TurnRun | null>(null);
  const [processDetailsByMessageId, setProcessDetailsByMessageId] = useState<Record<string, ProcessDetail[]>>({});
  const [completedRunsByMessageId, setCompletedRunsByMessageId] = useState<Record<string, TurnRun>>({});
  const abortControllersRef = useRef<Record<string, AbortController>>({});
  const handedOffRunIdsRef = useRef<Set<string>>(new Set());
  const conversationIdRef = useRef<string | undefined>(conversationId);
  const lastSseFallbackRef = useRef(0);
  const sseInvalidationTimersRef = useRef<Record<string, ReturnType<typeof setTimeout>>>({});
  const [runtime, runtimeDispatch] = useReducer(runtimeReducer, initialRuntimeState);
  const runtimeRef = useRef(runtime);
  const dispatch = useCallback((action: RuntimeAction) => {
    runtimeRef.current = runtimeReducer(runtimeRef.current, action);
    runtimeDispatch(action);
  }, []);
  const scheduleSseInvalidation = useCallback((slot: string, queryKey: readonly unknown[], delayMs = 1_500) => {
    if (sseInvalidationTimersRef.current[slot]) return;
    sseInvalidationTimersRef.current[slot] = setTimeout(() => {
      delete sseInvalidationTimersRef.current[slot];
      queryClient.invalidateQueries({ queryKey }).catch(() => undefined);
    }, delayMs);
  }, [queryClient]);

  useEffect(() => {
    return () => {
      Object.values(sseInvalidationTimersRef.current).forEach((timer) => clearTimeout(timer));
      sseInvalidationTimersRef.current = {};
    };
  }, []);

  useEffect(() => {
    runtimeRef.current = runtime;
  }, [runtime]);

  useEffect(() => {
    conversationIdRef.current = conversationId;
  }, [conversationId]);

  const data = useWorkbenchData(search, conversationId);
  const authRejected = [data.config.error, data.conversations.error, data.roles.error, data.projects.error].some(
    (error) => error instanceof ApiError && error.status === 401
  );
  const conversations = data.conversations.data?.conversations || EMPTY_CONVERSATIONS;
  const messages = data.conversation.data?.messages || EMPTY_MESSAGES;
  const roles = data.roles.data?.roles || EMPTY_ROLES;
  const projects = data.projects.data?.projects || EMPTY_PROJECTS;
  const activeTasks = data.tasks.data?.tasks || EMPTY_TASKS;
  const activeConversationRun = useMemo(() => {
    return displayRunForConversation(runtime.runs, conversationId);
  }, [conversationId, runtime.runs]);
  const activeConversationIsRunning = activeConversationRun?.status === "running" || activeConversationRun?.status === "awaiting_approval";
  const restoredEvents = useMemo(() => processDetailsToEvents(runtime.processDetails), [runtime.processDetails]);
  const latestAssistantMessage = useMemo(() => [...messages].reverse().find((m) => m.role === "assistant"), [messages]);
  const assistantMessageIds = useMemo(
    () => messages.filter((message) => message.role === "assistant" && message.id).map((message) => message.id),
    [messages]
  );
  const restoredRunsByMessageId = useMemo(() => {
    const runs: Record<string, TurnRun> = {};
    for (const message of messages) {
      if (message.role !== "assistant") continue;
      const details = processDetailsByMessageId[message.id] || message.processDetails || [];
      const events = processDetailsToEvents(details);
      const run = restoredRunFromDetails(conversationId, details, events, message);
      const completedRun = completedRunsByMessageId[message.id];
      if (run) runs[message.id] = mergeRunActivity(run, completedRun);
      else if (completedRun) runs[message.id] = completedRun;
    }
    return runs;
  }, [completedRunsByMessageId, conversationId, messages, processDetailsByMessageId]);
  const visibleRun = activeConversationRun;
  const traceEvents = activeConversationRun?.events?.length ? activeConversationRun.events : restoredEvents;
  const latestRestoredRun = latestAssistantMessage?.id ? restoredRunsByMessageId[latestAssistantMessage.id] : undefined;
  const todoDockRun = activeConversationRun?.plan.length ? activeConversationRun : latestRestoredRun?.plan.length ? latestRestoredRun : null;

  useEffect(() => {
    setProcessDetailsByMessageId({});
  }, [conversationId]);

  useEffect(() => {
    if (!conversationId || data.runtimeTodos.isLoading || !data.runtimeTodos.data) return;
    const plan = planItemsFromRuntimeTodos(data.runtimeTodos.data.todos || EMPTY_RUNTIME_TODOS);
    dispatch({ type: "plan", conversationId, items: plan, status: activeConversationIsRunning ? "running" : "completed" });
  }, [activeConversationIsRunning, conversationId, data.runtimeTodos.data, data.runtimeTodos.isLoading]);

  useEffect(() => {
    if (!conversationId || !data.pending.data?.items?.length) return;
    let run = runningRunForConversation(runtimeRef.current.runs, conversationId) || runtimeRef.current.runs.find((item) => item.conversationId === conversationId);
    if (!run) {
      dispatch({ type: "ensure_run", conversationId, origin: "task" });
      run = runningRunForConversation(runtimeRef.current.runs, conversationId) || runtimeRef.current.runs.find((item) => item.conversationId === conversationId);
    }
    for (const item of data.pending.data.items) {
      dispatch({ type: "approval", approval: item, runId: run?.id });
    }
  }, [conversationId, data.pending.data]);

  useEffect(() => {
    const activeConversationIds = new Set(
      activeTasks
        .map((task) => (typeof task.conversationId === "string" ? task.conversationId.trim() : ""))
        .filter(Boolean)
    );
    const completedTaskRuns = runtimeRef.current.runs.filter(
      (run) =>
        run.origin === "task" &&
        run.conversationId &&
        !activeConversationIds.has(run.conversationId) &&
        (run.status === "running" || run.status === "awaiting_approval")
    );
    dispatch({ type: "tasks", tasks: activeTasks });
    dispatch({ type: "hydrate_tasks", tasks: activeTasks });
    for (const run of completedTaskRuns) {
      if (run.conversationId) {
        scheduleSseInvalidation(`conversation:${run.conversationId}`, ["conversation", run.conversationId]);
      }
    }
    if (completedTaskRuns.length > 0) {
      scheduleSseInvalidation("conversations", ["conversations"]);
    }
  }, [activeTasks, scheduleSseInvalidation]);

  useEffect(() => {
    if (!conversationId && conversations[0]?.id) setConversationId(conversations[0].id);
  }, [conversationId, conversations]);

  useEffect(() => {
    const cfg = data.config.data;
    if (!cfg) return;
    setSelectedModel((prev) => prev || cfg.openai?.model || "");
    setReasoningEffort((prev) => normalizeReasoningEffort(prev || cfg.openai?.reasoning?.effort));
  }, [data.config.data]);

  useEffect(() => {
    const h = data.hitl.data?.hitl;
    if (h) setHitlConfig({ timeoutSeconds: 600, sensitiveTools: [], ...h });
  }, [data.hitl.data]);

  useEffect(() => {
    if (data.conversation.data?.projectId) setSelectedProject(data.conversation.data.projectId);
  }, [data.conversation.data?.projectId]);

  useEffect(() => {
    const assistantMessages = messages.filter((message) => message.role === "assistant" && message.id);
    if (assistantMessages.length === 0) {
      setProcessDetailsByMessageId({});
      return;
    }

    let cancelled = false;
    for (const message of assistantMessages) {
      Api.processDetails(message.id)
        .then((res) => {
          if (cancelled) return;
          setProcessDetailsByMessageId((prev) => {
            if ((prev[message.id]?.length || 0) >= res.processDetails.length) return prev;
            return { ...prev, [message.id]: res.processDetails };
          });
          if (message.id === latestAssistantMessage?.id) {
            dispatch({ type: "process_details", details: res.processDetails });
          }
        })
        .catch(() => undefined);
    }
    return () => {
      cancelled = true;
    };
  }, [assistantMessageIds, latestAssistantMessage?.id, messages]);

  useEffect(() => {
    const token = getAuthToken();
    const source = new EventSource(`/api/agent-loop/task-events${token ? `?token=${encodeURIComponent(token)}` : ""}`);
    source.onmessage = (evt) => {
      const parsed = parseSSELine(evt.data);
      if (!parsed) return;
      const cid = conversationIdFromEnvelope(parsed);
      if (!cid) return;
      const type = String(parsed.type || "");
      if (type === "task_updated" || type === "task_completed" || type === "task_removed") {
        const task = taskFromEnvelope(parsed);
        if (task) {
          const active = taskIsActive(task) && type === "task_updated";
          const currentTasks = queryClient.getQueryData<{ tasks: AgentTask[] }>(["tasks"])?.tasks || runtimeRef.current.activeTasks || [];
          const nextTasks = active
            ? [...currentTasks.filter((item) => item.conversationId !== task.conversationId), task]
            : currentTasks.filter((item) => item.conversationId !== task.conversationId);
          queryClient.setQueryData(["tasks"], { tasks: nextTasks });
          dispatch({ type: "tasks", tasks: nextTasks });
          if (active) {
            dispatch({
              type: "ensure_run",
              conversationId: cid,
              message: task.message,
              startedAt: taskStartedAt(task),
              origin: "task"
            });
          } else {
            const run = runningRunForConversation(runtimeRef.current.runs, cid) || runtimeRef.current.runs.find((item) => item.conversationId === cid);
            dispatch({
              type: "finish",
              status: type === "task_removed" || task.status === "cancelled" || task.status === "canceled" ? "cancelled" : task.status === "failed" || task.status === "error" ? "error" : "completed",
              runId: run?.id
            });
          }
        }
        if (!task || !taskIsActive(task) || type !== "task_updated") {
          if (cid === conversationIdRef.current) {
            scheduleSseInvalidation(`conversation:${cid}`, ["conversation", cid]);
          }
          scheduleSseInvalidation("conversations", ["conversations"]);
        }
        return;
      }
      if (type === "hitl_pending_updated" || type === "hitl_decision_updated") {
        const items = Array.isArray(parsed.data?.items) ? parsed.data.items as HITLPendingItem[] : [];
        queryClient.setQueryData(["hitl-pending", cid], { items });
        if (cid === conversationIdRef.current) {
          let run = runningRunForConversation(runtimeRef.current.runs, cid) || runtimeRef.current.runs.find((item) => item.conversationId === cid);
          if (!run && items.length > 0) {
            dispatch({ type: "ensure_run", conversationId: cid, origin: "task" });
            run = runningRunForConversation(runtimeRef.current.runs, cid) || runtimeRef.current.runs.find((item) => item.conversationId === cid);
          }
          for (const item of items) dispatch({ type: "approval", approval: item, runId: run?.id });
          if (items.length === 0) {
            const run = runningRunForConversation(runtimeRef.current.runs, cid);
            if (run?.status === "awaiting_approval") dispatch({ type: "event", runId: run.id, event: { id: `hitl-${Date.now()}`, type, label: type, time: new Date().toISOString(), raw: parsed }, patch: { status: "running" } });
          }
        }
        return;
      }
      if (type === "todo_updated" || type === "todo_cleared") {
        const todos = type === "todo_cleared" ? [] : runtimeTodoItemsFromUnknown(parsed.data?.todos || parsed.data?.items);
        queryClient.setQueryData(["runtime-todos", cid], { conversationId: cid, todos });
        dispatch({
          type: "plan",
          conversationId: cid,
          items: planItemsFromRuntimeTodos(todos),
          status: runningRunForConversation(runtimeRef.current.runs, cid) ? "running" : "completed"
        });
        return;
      }
      if (parsed.type === "conversation_title_updated") {
        if (cid === conversationIdRef.current) {
          scheduleSseInvalidation(`conversation:${cid}`, ["conversation", cid]);
        }
        scheduleSseInvalidation("conversations", ["conversations"]);
        return;
      }
      let scopedRun = runningRunForConversation(runtimeRef.current.runs, cid) ||
        recentCompletedRunForConversation(runtimeRef.current.runs, cid);
      if (!scopedRun) {
        const task = runtimeRef.current.activeTasks.find((item) => item.conversationId === cid);
        dispatch({
          type: "ensure_run",
          conversationId: cid,
          message: task?.message,
          startedAt: task ? taskStartedAt(task) : undefined,
          origin: "task"
        });
        scopedRun = runningRunForConversation(runtimeRef.current.runs, cid);
      }
      const scopedRunId = scopedRun?.id || runtimeRef.current.runs.find((run) => run.conversationId === cid)?.id;
      adaptSSE(parsed).forEach((action) => dispatch({ ...action, runId: scopedRunId } as RuntimeAction));
      if (isTerminalRunEnvelope(parsed)) {
        if (cid === conversationIdRef.current) {
          scheduleSseInvalidation(`conversation:${cid}`, ["conversation", cid]);
        }
        scheduleSseInvalidation("conversations", ["conversations"]);
        scheduleSseInvalidation("tasks", ["tasks"]);
      }
    };
    source.onerror = () => {
      const now = Date.now();
      if (now - lastSseFallbackRef.current < 30_000) return;
      lastSseFallbackRef.current = now;
      queryClient.invalidateQueries({ queryKey: ["tasks"] }).catch(() => undefined);
      const cid = conversationIdRef.current;
      if (cid) {
        queryClient.invalidateQueries({ queryKey: ["hitl-pending", cid] }).catch(() => undefined);
        queryClient.invalidateQueries({ queryKey: ["runtime-todos", cid] }).catch(() => undefined);
      }
    };
    return () => source.close();
  }, [queryClient, dispatch, scheduleSseInvalidation]);

  const liveMessages = useMemo(() => {
    if (!activeConversationRun) return messages;
    const nextMessages = [...messages];
    const runIsStreaming = activeConversationRun.status === "running" || activeConversationRun.status === "awaiting_approval";
    const userText = activeConversationRun.userMessage?.trim();
    const hasUserMessage = userText
      ? nextMessages.some((message) => message.role === "user" && message.content.trim() === userText)
      : true;
    if (runIsStreaming && userText && !hasUserMessage) {
      nextMessages.push({
        id: `${activeConversationRun.id}-user`,
        conversationId: conversationId || "",
        role: "user",
        content: userText,
        createdAt: activeConversationRun.startedAt
      });
    }
    const placeholderIndex = nextMessages.findIndex(
      (message) => message.role === "assistant" && isAssistantPlaceholder(message.content)
    );
    const assistantText = activeConversationRun.assistantText.trim();
    if (!assistantText) {
      return runIsStreaming && placeholderIndex >= 0
        ? nextMessages.filter((_, index) => index !== placeholderIndex)
        : nextMessages;
    }
    if (placeholderIndex >= 0) {
      nextMessages[placeholderIndex] = {
        ...nextMessages[placeholderIndex],
        content: assistantText,
        reasoningContent: activeConversationRun.reasoningText || nextMessages[placeholderIndex].reasoningContent
      };
      return nextMessages;
    }
    if (nextMessages.some((message) => message.role === "assistant" && message.content.trim() === assistantText)) {
      return nextMessages;
    }
    const draft: ConversationMessage = {
      id: `${activeConversationRun.id}-assistant`,
      conversationId: conversationId || "",
      role: "assistant",
      content: assistantText,
      createdAt: activeConversationRun.startedAt
    };
    return [...nextMessages, draft];
  }, [activeConversationRun, conversationId, messages]);

  async function ensureConversation() {
    if (conversationId) return conversationId;
    const created = await data.createConversation.mutateAsync();
    setConversationId(created.id);
    return created.id;
  }

  async function sendMessage() {
    const text = input.trim();
    if (!text || activeConversationIsRunning) return;
    const cid = await ensureConversation();
    const runId = `run-${Date.now()}-${Math.random().toString(16).slice(2)}`;
    const startedAt = new Date().toISOString();
    const existingAssistantIds = new Set(messages.filter((message) => message.role === "assistant").map((message) => message.id));
    setInput("");
    dispatch({ type: "start", id: runId, startedAt, conversationId: cid, message: text });
    const controller = new AbortController();
    abortControllersRef.current[runId] = controller;

    const payload: ChatRequest = {
      message: text,
      conversationId: cid,
      projectId: selectedProject || undefined,
      role: selectedRole || undefined,
      reasoning: { effort: normalizeReasoningEffort(reasoningEffort) },
      hitl: hitlConfig,
      background: true
    };

    try {
      await apiStream(
        "/api/agent-runtime/stream",
        Api.streamPayload(payload),
        (line) => {
          const parsed = parseSSELine(line);
          if (parsed) {
            const startupDone = parsed.type === "done" && parsed.data?.background === true;
            const backgroundAccepted = parsed.data?.background === true && parsed.type === "runtime_status_update";
            const titleConversationId = parsed.type === "conversation_title_updated" ? conversationIdFromEnvelope(parsed) : "";
            if (titleConversationId) {
              queryClient.invalidateQueries({ queryKey: ["conversation", titleConversationId] }).catch(() => undefined);
              queryClient.invalidateQueries({ queryKey: ["conversations"] }).catch(() => undefined);
              return;
            }
            if (!startupDone) {
              adaptSSE(parsed).forEach((action) => dispatch({ ...action, runId } as RuntimeAction));
            }
            if (
              (backgroundAccepted || isAcceptedRunEnvelope(parsed)) &&
              !isTerminalRunEnvelope(parsed)
            ) {
              handedOffRunIdsRef.current.add(runId);
              dispatch({ type: "adopt_task", runId });
            }
          }
        },
        controller.signal
      );
      if (handedOffRunIdsRef.current.has(runId)) {
        await queryClient.invalidateQueries({ queryKey: ["tasks"] }).catch(() => undefined);
        return;
      }
      const refreshedConversation = await Api.conversation(cid).catch(() => null);
      if (refreshedConversation) {
        queryClient.setQueryData(["conversation", cid], refreshedConversation);
      }
      await queryClient.invalidateQueries({ queryKey: ["conversation", cid] }).catch(() => undefined);
      await data.conversations.refetch();
      const assistantMessages = refreshedConversation?.messages?.filter((message) => message.role === "assistant") || [];
      const assistantMessage =
        [...assistantMessages].reverse().find((message) => !existingAssistantIds.has(message.id)) ||
        assistantMessages[assistantMessages.length - 1];
      const completedRun = runtimeRef.current.runs.find((run) => run.id === runId);
      if (assistantMessage && completedRun) {
        setCompletedRunsByMessageId((prev) => ({
          ...prev,
          [assistantMessage.id]: {
            ...completedRun,
            id: `completed-${assistantMessage.id}`,
            conversationId: cid,
            assistantText: isAssistantPlaceholder(assistantMessage.content) ? completedRun.assistantText : assistantMessage.content || completedRun.assistantText,
            reasoningText: assistantMessage.reasoningContent || completedRun.reasoningText,
            progressUpdates: completedRun.progressUpdates,
            status: completedRun.status === "running" || completedRun.status === "awaiting_approval" ? "completed" : completedRun.status,
            completedAt: completedRun.completedAt || new Date().toISOString()
          }
        }));
      }
      dispatch({ type: "finish", status: "completed", runId });
    } catch (error) {
      if (controller.signal.aborted && handedOffRunIdsRef.current.has(runId)) {
        await queryClient.invalidateQueries({ queryKey: ["tasks"] }).catch(() => undefined);
      } else if (controller.signal.aborted) {
        dispatch({ type: "finish", status: "cancelled", runId });
      } else {
        dispatch({ type: "finish", status: "error", error: error instanceof Error ? error.message : String(error), runId });
      }
    } finally {
      handedOffRunIdsRef.current.delete(runId);
      delete abortControllersRef.current[runId];
    }
  }

  async function stopRun() {
    const cid = conversationIdRef.current;
    const run = [...runtimeRef.current.runs].find(
      (item) => item.conversationId === cid && (item.status === "running" || item.status === "awaiting_approval")
    );
    const targetConversationId = run?.conversationId || cid;
    if (targetConversationId) await Api.cancel(targetConversationId).catch(() => undefined);
    if (run?.id) abortControllersRef.current[run.id]?.abort();
    dispatch({ type: "finish", status: "cancelled", runId: run?.id });
  }

  async function applyModel(model: string) {
    setSelectedModel(model);
    await data.updateConfig.mutateAsync(updateOpenAIModel(data.config.data, model));
  }

  async function fetchModels() {
    setModelError("");
    try {
      const res = await data.listModels.mutateAsync();
      if (!res.success) {
        setModelError(res.error || "Failed to fetch models");
        return;
      }
      const models = normalizeModelOptions(res.models);
      setModelOptions(models);
      if (!models.length) setModelError("No models returned by Base API");
    } catch (error) {
      setModelError(error instanceof Error ? error.message : String(error));
    }
  }

  async function saveHitl(next = hitlConfig) {
    const cid = await ensureConversation();
    setHitlConfig(next);
    await data.saveHitl.mutateAsync({ id: cid, hitl: next });
  }

  async function newChat() {
    const conv = await data.createConversation.mutateAsync();
    setConversationId(conv.id);
    setInput("");
  }

  async function selectConversation(id: string) {
    if (!id || id === conversationId) return;
    const prefetched = await Api.conversation(id).catch(() => null);
    if (prefetched) {
      queryClient.setQueryData(["conversation", id], prefetched);
    }
    setConversationId(id);
  }

  const authError = authRejected ? "后端返回 401，当前请求未通过 API 认证" : "";

  return (
    <div className="dark h-screen overflow-hidden bg-[#07080b] text-zinc-100">
      <div className="absolute inset-0 bg-[radial-gradient(circle_at_16%_8%,rgba(95,116,146,0.28),transparent_30%),radial-gradient(circle_at_84%_20%,rgba(92,73,115,0.16),transparent_32%),linear-gradient(135deg,#0b0d12,#101113_45%,#08090c)]" />
      <div className="relative flex h-full p-3">
        <Rail />
        <ConversationSidebar
          conversations={conversations}
          selectedId={conversationId}
          search={search}
          onSearch={setSearch}
          onNew={newChat}
          onSelect={selectConversation}
          onRename={(id, title) => data.renameConversation.mutate({ id, title })}
          onDelete={async (id) => {
            await data.deleteConversation.mutateAsync(id);
            if (id === conversationId) setConversationId(undefined);
          }}
        />
        <main className="chat-surface ml-3 flex min-w-0 flex-1 flex-col overflow-hidden rounded-[30px] max-sm:ml-0">
          {authError && (
            <div className="mx-5 mt-4 rounded-2xl border border-amber-400/20 bg-amber-400/10 px-4 py-2 text-sm text-amber-100">
              {authError}。如果后端认证已关闭，这条提示不会出现；如果后端开启认证，新前端只会复用{" "}
              <span className="font-medium">localStorage.cyberstrike-auth</span>，不会跳转登录。
            </div>
          )}
          <Header
            title={data.conversation.data?.title || "New Chat"}
            projectName={projects.find((p) => p.id === selectedProject)?.name}
            status={activeConversationRun?.status || "idle"}
          />
          <Transcript
            messages={liveMessages}
            activeRun={visibleRun}
            runsByMessageId={restoredRunsByMessageId}
            onHoverRun={setHoveredRun}
            onLeaveRun={() => setHoveredRun(null)}
          />
          <RunTodoDock run={todoDockRun} />
          <Composer
            value={input}
            onChange={setInput}
            onSend={sendMessage}
            onStop={stopRun}
            isRunning={activeConversationIsRunning}
            roles={roles}
            selectedRole={selectedRole}
            onRole={setSelectedRole}
            projects={projects}
            selectedProject={selectedProject}
            onProject={async (projectId) => {
              setSelectedProject(projectId);
              if (conversationId && projectId) await data.setProject.mutateAsync({ id: conversationId, projectId });
            }}
            model={selectedModel}
            modelOptions={modelOptions}
            modelError={modelError}
            onModel={applyModel}
            onFetchModels={fetchModels}
            fetchingModels={data.listModels.isPending}
            reasoningEffort={reasoningEffort}
            onReasoningEffort={setReasoningEffort}
            hitl={hitlConfig}
            onHitl={saveHitl}
          />
        </main>
        <TraceRail
          open={traceOpen}
          setOpen={setTraceOpen}
          events={traceEvents}
          tasks={runtime.activeTasks}
          details={runtime.processDetails}
        />
      </div>
    </div>
  );
}

function Rail() {
  return (
    <aside className="app-rail glass-panel flex w-[68px] shrink-0 flex-col items-center rounded-[28px] py-3">
      <div className="mb-5 flex gap-1.5">
        <span className="h-3 w-3 rounded-full bg-red-400/[.90]" />
        <span className="h-3 w-3 rounded-full bg-amber-300/[.90]" />
        <span className="h-3 w-3 rounded-full bg-emerald-400/[.90]" />
      </div>
      <div className="flex flex-1 flex-col gap-2">
        {navItems.map((item) => {
          const Icon = item.icon;
          return (
            <Tooltip key={item.id}>
              <TooltipTrigger asChild>
                <button
                  className={cn(
                    "flex h-11 w-11 items-center justify-center rounded-2xl text-zinc-400 transition",
                    item.id === "chat" ? "bg-white/[.14] text-white shadow-glow" : "hover:bg-white/[.09] hover:text-zinc-100"
                  )}
                >
                  <Icon className="h-5 w-5" />
                </button>
              </TooltipTrigger>
              <TooltipContent side="right" className="rounded-xl border border-white/10 bg-zinc-950/[.90] px-2 py-1 text-xs text-zinc-100">
                {item.label}
              </TooltipContent>
            </Tooltip>
          );
        })}
      </div>
    </aside>
  );
}

function ConversationSidebar(props: {
  conversations: { id: string; title: string; updatedAt?: string; pinned?: boolean }[];
  selectedId?: string;
  search: string;
  onSearch: (value: string) => void;
  onNew: () => void;
  onSelect: (id: string) => void;
  onRename: (id: string, title: string) => void;
  onDelete: (id: string) => void;
}) {
  const groups = groupConversations(props.conversations);
  let lastGroup = "";
  return (
    <aside className="conversation-sidebar glass-panel ml-3 flex w-[292px] shrink-0 flex-col rounded-[30px] p-4">
      <div className="mb-3 flex items-center justify-between">
        <div>
          <div className="text-sm font-semibold text-white">Conversations</div>
          <div className="text-xs text-zinc-500">Agent Runtime</div>
        </div>
        <Button data-testid="new-chat" variant="glass" size="icon" onClick={props.onNew}>
          <Plus className="h-4 w-4" />
        </Button>
      </div>
      <div className="relative mb-3">
        <Search className="pointer-events-none absolute left-3 top-2.5 h-4 w-4 text-zinc-500" />
        <Input className="pl-9" placeholder="Search chats" value={props.search} onChange={(e) => props.onSearch(e.target.value)} />
      </div>
      <ScrollArea className="min-h-0 min-w-0 flex-1 pr-1">
        <div className="w-full min-w-0 space-y-1">
          {props.conversations.map((conv, index) => {
            const group = groups[index];
            const showGroup = group !== lastGroup;
            lastGroup = group;
            return (
              <div key={conv.id} className="min-w-0">
                {showGroup && <div className="px-2 pb-1 pt-3 text-[11px] font-medium uppercase text-zinc-500">{group}</div>}
                <ConversationRow
                  id={conv.id}
                  conversation={conv}
                  active={conv.id === props.selectedId}
                  onSelect={() => props.onSelect(conv.id)}
                  onRename={(title) => props.onRename(conv.id, title)}
                  onDelete={() => props.onDelete(conv.id)}
                />
              </div>
            );
          })}
        </div>
      </ScrollArea>
    </aside>
  );
}

function ConversationRow(props: {
  id: string;
  conversation: { title: string; updatedAt?: string; pinned?: boolean };
  active: boolean;
  onSelect: () => void;
  onRename: (title: string) => void;
  onDelete: () => void;
}) {
  const [renaming, setRenaming] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [title, setTitle] = useState(props.conversation.title);
  return (
    <div
      className={cn(
        "group flex items-center gap-2 rounded-2xl px-2 py-1.5 transition",
        props.active ? "bg-white/[.13] text-white" : "text-zinc-300 hover:bg-white/[.07]"
      )}
    >
      <button
        type="button"
        data-testid={`conversation-row-${props.id}`}
        className="flex min-w-0 flex-1 cursor-pointer items-center gap-2 rounded-xl px-1 py-1 text-left"
        onClick={props.onSelect}
      >
        <MessageSquare className="h-4 w-4 shrink-0 text-zinc-500" />
        <div className="min-w-0 flex-1">
          {renaming ? (
            <input
              autoFocus
              className="w-full bg-transparent text-sm outline-none"
              value={title}
              onClick={(e) => e.stopPropagation()}
              onChange={(e) => setTitle(e.target.value)}
              onBlur={() => {
                setRenaming(false);
                if (title.trim()) props.onRename(title.trim());
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") e.currentTarget.blur();
                if (e.key === "Escape") setRenaming(false);
              }}
            />
          ) : (
            <div className="truncate text-sm">{props.conversation.title}</div>
          )}
          <div className="text-[11px] text-zinc-500">{formatTime(props.conversation.updatedAt)}</div>
        </div>
      </button>
      <Popover>
        <PopoverTrigger asChild>
          <button
            type="button"
            data-testid={`conversation-menu-${props.id}`}
            className="rounded-full p-1 opacity-0 transition hover:bg-white/10 group-hover:opacity-100"
            onClick={(e) => e.stopPropagation()}
          >
            <MoreHorizontal className="h-4 w-4" />
          </button>
        </PopoverTrigger>
        <PopoverContent className="w-36 p-1" align="end">
          <button
            data-testid={`conversation-rename-${props.id}`}
            className="flex w-full items-center gap-2 rounded-xl px-3 py-2 text-left text-xs hover:bg-white/10"
            onClick={(e) => {
              e.stopPropagation();
              setRenaming(true);
            }}
          >
            <UserRound className="h-3.5 w-3.5" /> Rename
          </button>
          <button
            data-testid={`conversation-delete-${props.id}`}
            className="flex w-full items-center gap-2 rounded-xl px-3 py-2 text-left text-xs text-red-200 hover:bg-red-500/[.14]"
            onClick={(e) => {
              e.stopPropagation();
              setConfirmDelete(true);
            }}
          >
            <Trash2 className="h-3.5 w-3.5" /> Delete
          </button>
        </PopoverContent>
      </Popover>
      <Dialog open={confirmDelete} onOpenChange={setConfirmDelete}>
        <DialogContent onClick={(e) => e.stopPropagation()}>
          <DialogHeader>
            <DialogTitle>Delete conversation</DialogTitle>
            <DialogDescription>
              This removes "{props.conversation.title}" from the conversation list.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="subtle" onClick={() => setConfirmDelete(false)}>Cancel</Button>
            <Button
              data-testid={`conversation-confirm-delete-${props.id}`}
              variant="danger"
              onClick={() => {
                setConfirmDelete(false);
                props.onDelete();
              }}
            >
              Delete
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

function Header({ title, projectName, status }: { title: string; projectName?: string; status: string }) {
  return (
    <header className="flex h-[72px] shrink-0 items-center justify-between border-b border-white/[.08] px-6">
      <div className="min-w-0">
        <h1 className="truncate text-[17px] font-semibold text-white">{title}</h1>
        <div className="mt-1 flex items-center gap-2 text-xs text-zinc-500">
          <Badge>{projectName || "No Project"}</Badge>
          <span className="capitalize">{status.replace("_", " ")}</span>
        </div>
      </div>
      <div className="flex items-center gap-2 text-xs text-zinc-500">
        <span className="h-2 w-2 rounded-full bg-emerald-400" />
        Runtime attached
      </div>
    </header>
  );
}

function Transcript({
  messages,
  activeRun,
  runsByMessageId,
  onHoverRun,
  onLeaveRun
}: {
  messages: ConversationMessage[];
  activeRun: TurnRun | null;
  runsByMessageId: Record<string, TurnRun>;
  onHoverRun: (run: TurnRun) => void;
  onLeaveRun: () => void;
}) {
  const userIndexes = messages
    .map((message, index) => (message.role === "user" ? index : -1))
    .filter((index) => index >= 0);
  const runningInsertIndex = userIndexes.length ? userIndexes[userIndexes.length - 1] : -1;
  const isRunning = activeRun?.status === "running" || activeRun?.status === "awaiting_approval";
  const activeRunRestoredMessageId = activeRunAssistantMessageId(messages, activeRun, runsByMessageId);
  const mergedRunsByMessageId = activeRunRestoredMessageId && activeRun
    ? {
        ...runsByMessageId,
        [activeRunRestoredMessageId]: runsByMessageId[activeRunRestoredMessageId]
          ? mergeRunActivity(runsByMessageId[activeRunRestoredMessageId], activeRun)
          : activeRun
      }
    : runsByMessageId;
  const shouldRenderActiveRun = Boolean(activeRun && isRunning && !activeRunRestoredMessageId);

  return (
    <ScrollArea className="min-h-0 flex-1">
      <div className="codex-transcript mx-auto flex w-full max-w-[1480px] flex-col gap-8 px-14 pb-48 pt-10 max-lg:px-8 max-sm:px-4 max-sm:pb-72">
        {messages.length === 0 && (
          <div className="mx-auto mt-16 max-w-lg text-center">
            <div className="mx-auto mb-5 flex h-14 w-14 items-center justify-center rounded-3xl border border-white/[.12] bg-white/[.08] shadow-glass">
              <Code2 className="h-7 w-7 text-zinc-100" />
            </div>
            <div className="text-xl font-semibold text-white">CyberStrikeAI</div>
            <div className="mt-2 text-sm leading-6 text-zinc-500">Start a runtime-backed security conversation.</div>
          </div>
        )}
        {messages.map((message, index) => (
          <Fragment key={message.id}>
            {message.role === "assistant" && mergedRunsByMessageId[message.id] && !(shouldRenderActiveRun && message.id === activeRunRestoredMessageId) && (
              <InlineRunActivity
                run={mergedRunsByMessageId[message.id]}
                onHover={() => onHoverRun(mergedRunsByMessageId[message.id])}
                onLeave={onLeaveRun}
                onInspect={() => onHoverRun(mergedRunsByMessageId[message.id])}
              />
            )}
            <MessageBubble message={message} />
            {activeRun && index === runningInsertIndex && shouldRenderActiveRun && (
              <InlineRunActivity
                run={activeRun}
                onHover={() => onHoverRun(activeRun)}
                onLeave={onLeaveRun}
                onInspect={() => onHoverRun(activeRun)}
              />
            )}
          </Fragment>
        ))}
        <div className="h-px w-full shrink-0" />
      </div>
    </ScrollArea>
  );
}

function InlineRunActivity({
  run,
  onHover,
  onLeave,
  onInspect
}: {
  run: TurnRun;
  onHover: () => void;
  onLeave: () => void;
  onInspect: () => void;
}) {
  const isRunning = run.status === "running" || run.status === "awaiting_approval";
  const elapsed = runElapsedLabel(run);
  const statusLabel = runStatusLabel(run);
  const summaryLabel = [statusLabel, elapsed].filter(Boolean).join(" ");
  const items = runActivityItems(run);
  const [expanded, setExpanded] = useState(false);
  const activityCount = items.length;

  return (
    <div className={cn("codex-run-block w-full", expanded && "codex-run-block-expanded")} onMouseEnter={onHover} onMouseLeave={onLeave}>
      <button
        data-testid="assistant-run-activity"
        type="button"
        className="codex-run-summary flex items-center gap-2"
        aria-expanded={expanded}
        onClick={() => {
          setExpanded((value) => !value);
        }}
      >
        <span>{summaryLabel || (isRunning ? "处理中" : "已处理")}</span>
        {activityCount > 0 && <span className="codex-run-summary-count">{activityCount} 项活动</span>}
        <ChevronRight className={cn("h-4 w-4 transition", expanded && "rotate-90")} />
      </button>
      {expanded && (
        <>
          {items.length > 0 && (
            <div data-testid="assistant-progress-updates" className="codex-progress-updates">
              {items.map((item) => (
                <RunActivityItemRow
                  key={item.id}
                  item={item}
                  running={isRunning && item.id === items[items.length - 1]?.id}
                  onInspect={onInspect}
                />
              ))}
            </div>
          )}
        </>
      )}
    </div>
  );
}

function RunTodoDock({ run }: { run: TurnRun | null }) {
  if (!run) return null;
  return (
    <div className="codex-todo-dock shrink-0 px-5">
      <RunTodoPill run={run} activityCount={runActivityItems(run).length} />
    </div>
  );
}

function RunTodoPill({ run, activityCount }: { run: TurnRun; activityCount: number }) {
  const progress = todoProgress(run.plan);
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  const [pinned, setPinned] = useState(false);
  const closeTimerRef = useRef<number | null>(null);
  const clearCloseTimer = () => {
    if (closeTimerRef.current == null) return;
    window.clearTimeout(closeTimerRef.current);
    closeTimerRef.current = null;
  };
  const scheduleClose = () => {
    clearCloseTimer();
    closeTimerRef.current = window.setTimeout(() => {
      setHovered(false);
      setFocused(false);
    }, 140);
  };
  useEffect(() => () => clearCloseTimer(), []);
  if (!progress) return null;
  const open = pinned || hovered || focused;
  const stepNumber = Math.min(progress.currentIndex + 1, progress.total);
  const active = run.plan.find((item) => item.status === "in_progress") || progress.current;
  return (
    <Popover
      open={open}
      onOpenChange={(nextOpen) => {
        if (!nextOpen) {
          setPinned(false);
          setHovered(false);
          setFocused(false);
        }
      }}
    >
      <PopoverTrigger asChild>
        <button
          data-testid="run-todo-pill"
          type="button"
          aria-label="打开 Todo 进度"
          className="codex-step-pill inline-flex items-center gap-2"
          onClick={(event) => {
            event.preventDefault();
            clearCloseTimer();
            setPinned((value) => !value);
          }}
          onMouseEnter={() => {
            clearCloseTimer();
            setHovered(true);
          }}
          onMouseLeave={scheduleClose}
          onFocus={() => {
            clearCloseTimer();
            setFocused(true);
          }}
          onBlur={scheduleClose}
        >
          <TodoStatusDot status={active?.status || "pending"} />
          <span>第 {stepNumber} / {progress.total} 步</span>
          {activityCount > 0 && <span className="codex-step-pill-muted">· {activityCount} 项活动</span>}
        </button>
      </PopoverTrigger>
      <PopoverContent
        data-testid="run-todo-popover"
        className="codex-todo-popover w-[min(32rem,calc(100vw-2rem))] border-0 p-3.5 text-zinc-100"
        align="center"
        side="top"
        sideOffset={10}
        onMouseEnter={() => {
          clearCloseTimer();
          setHovered(true);
        }}
        onMouseLeave={scheduleClose}
        onOpenAutoFocus={(event) => event.preventDefault()}
      >
        <div className="space-y-2.5">
          {run.plan.map((item) => (
            <div key={item.id} className="codex-todo-popover-row flex items-start gap-2.5">
              <TodoStatusDot status={item.status} />
              <span className={cn("min-w-0 flex-1", item.status === "completed" && "text-zinc-500")}>{item.content}</span>
            </div>
          ))}
        </div>
      </PopoverContent>
    </Popover>
  );
}

function AssistantProgressUpdateRow({ update, running }: { update: ProgressUpdate; running: boolean }) {
  return (
    <div data-testid="assistant-progress-update" className="codex-assistant-progress-row">
      <span
        aria-hidden="true"
        className={cn(
          "codex-progress-ring mt-[0.18rem] shrink-0",
          running ? "codex-progress-ring-running" : "codex-progress-ring-complete"
        )}
      />
      <span className="min-w-0 flex-1">{progressPreview(update.message)}</span>
    </div>
  );
}

function RunActivityItemRow({
  item,
  running,
  onInspect
}: {
  item: RunActivityItem;
  running: boolean;
  onInspect: () => void;
}) {
  if (item.kind === "progress") return <AssistantProgressUpdateRow update={item.update} running={running} />;
  return <RuntimeCellRow cell={item.cell} onInspect={onInspect} />;
}

function RuntimeCellRow({
  cell,
  onInspect
}: {
  cell: RuntimeCell;
  onInspect: () => void;
}) {
  const [open, setOpen] = useState(false);
  const hasDetail = Boolean(cell.detail?.trim());
  const Icon = cell.kind === "edit" ? FilePenLine : cell.kind === "knowledge" ? Brain : cell.kind === "approval" ? ShieldCheck : Code2;
  const statusText =
    cell.status === "running"
      ? "运行中"
      : cell.status === "failed"
        ? "失败"
        : cell.status === "cancelled"
          ? "已取消"
          : "";

  return (
    <details
      className="codex-runtime-cell group/cell"
      open={open}
      onToggle={(event) => {
        setOpen(event.currentTarget.open);
      }}
    >
      <summary
        data-testid="runtime-cell"
        className="codex-runtime-cell-summary flex cursor-pointer list-none items-center gap-3"
        onClick={(event) => {
          if (!hasDetail) {
            event.preventDefault();
            onInspect();
          }
        }}
      >
        <Icon className="h-[18px] w-[18px] shrink-0 text-zinc-500" />
        <span className="min-w-0 flex-1 truncate">{cell.label}</span>
        {statusText && <span className="shrink-0 text-[13px] text-zinc-500">{statusText}</span>}
        {hasDetail && <ChevronDown className="h-4 w-4 shrink-0 text-zinc-600 transition group-open/cell:rotate-180" />}
      </summary>
      {hasDetail && open && (
        <div className="codex-runtime-cell-detail">
          <pre>{cell.detail}</pre>
        </div>
      )}
    </details>
  );
}

function MessageBubble({ message }: { message: ConversationMessage }) {
  const isUser = message.role === "user";
  return (
    <div className={cn("codex-message flex", isUser ? "justify-end" : "justify-start")}>
      <div className={cn(isUser ? "codex-user-bubble" : "codex-assistant-message")}>
        <div
          className={cn("prose chat-prose max-w-none", isUser ? "codex-user-text" : "codex-assistant-text")}
          dangerouslySetInnerHTML={{ __html: markdownHtml(message.content || "") }}
        />
      </div>
    </div>
  );
}

function Composer(props: {
  value: string;
  onChange: (value: string) => void;
  onSend: () => void;
  onStop: () => void;
  isRunning: boolean;
  roles: { name: string; description?: string }[];
  selectedRole: string;
  onRole: (value: string) => void;
  projects: { id: string; name: string }[];
  selectedProject: string;
  onProject: (value: string) => void;
  model: string;
  modelOptions: string[];
  modelError: string;
  onModel: (value: string) => void;
  onFetchModels: () => void;
  fetchingModels: boolean;
  reasoningEffort: string;
  onReasoningEffort: (value: string) => void;
  hitl: HITLConfig;
  onHitl: (value: HITLConfig) => void;
}) {
  return (
    <div className="shrink-0 px-5 pb-5">
      <div className="mac-composer mx-auto max-w-[980px] rounded-[30px] p-3">
        <Textarea
          className="text-zinc-100 placeholder:text-zinc-500"
          data-testid="chat-composer-input"
          id="chat-web-message"
          name="message"
          value={props.value}
          placeholder="Message CyberStrikeAI..."
          onChange={(e) => props.onChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              props.onSend();
            }
          }}
        />
        <div className="mt-2 flex flex-wrap items-center gap-2">
          <Select value={props.selectedRole || "default"} onValueChange={(v) => props.onRole(v === "default" ? "" : v)}>
            <SelectTrigger><SelectValue placeholder="Role" /></SelectTrigger>
            <SelectContent>
              <SelectItem value="default">Default Role</SelectItem>
              {props.roles.map((role) => <SelectItem key={role.name} value={role.name}>{role.name}</SelectItem>)}
            </SelectContent>
          </Select>
          <Select value={props.selectedProject || "none"} onValueChange={(v) => props.onProject(v === "none" ? "" : v)}>
            <SelectTrigger><SelectValue placeholder="Project" /></SelectTrigger>
            <SelectContent>
              <SelectItem value="none">No Project</SelectItem>
              {props.projects.map((project) => <SelectItem key={project.id} value={project.id}>{project.name}</SelectItem>)}
            </SelectContent>
          </Select>
          <ModelControl
            model={props.model}
            modelOptions={props.modelOptions}
            modelError={props.modelError}
            onModel={props.onModel}
            onFetchModels={props.onFetchModels}
            fetchingModels={props.fetchingModels}
          />
          <ReasoningControl
            effort={props.reasoningEffort}
            onEffort={props.onReasoningEffort}
          />
          <HitlControl hitl={props.hitl} onHitl={props.onHitl} />
          <div className="ml-auto flex items-center gap-2">
            {props.isRunning ? (
              <Button data-testid="stop-run" variant="danger" size="icon" onClick={props.onStop}><CircleStop className="h-4 w-4" /></Button>
            ) : (
              <Button data-testid="send-message" size="icon" onClick={props.onSend} disabled={!props.value.trim()}><Send className="h-4 w-4" /></Button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function ModelControl({
  model,
  modelOptions,
  modelError,
  onModel,
  onFetchModels,
  fetchingModels
}: {
  model: string;
  modelOptions: string[];
  modelError: string;
  onModel: (value: string) => void;
  onFetchModels: () => void;
  fetchingModels: boolean;
}) {
  const [draft, setDraft] = useState(model);

  useEffect(() => {
    setDraft(model);
  }, [model]);

  const commitDraft = () => {
    const next = draft.trim();
    if (next && next !== model) onModel(next);
  };

  return (
    <Popover>
      <PopoverTrigger asChild>
        <Button data-testid="model-control" variant="subtle" size="sm" className="composer-token">
          <Bot className="h-3.5 w-3.5" />
          <span className="composer-token-label">Model</span>
          <span className="composer-token-separator">·</span>
          <span className="composer-token-value max-w-[9.5rem]">{model || "Select"}</span>
        </Button>
      </PopoverTrigger>
      <PopoverContent className="composer-popover w-[360px] p-4" align="start">
        <div className="composer-popover-title">Model</div>
        <Input
          data-testid="model-input"
          className="composer-field h-10 text-[13px] font-medium"
          value={draft}
          onBlur={commitDraft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              commitDraft();
            }
          }}
          placeholder="model id"
        />
        <Button
          data-testid="fetch-models"
          variant="glass"
          size="default"
          className="composer-action-button mt-3 h-10 w-full"
          onClick={onFetchModels}
          disabled={fetchingModels}
        >
          {fetchingModels ? "Fetching models..." : "Fetch models"}
        </Button>
        {modelError && <div className="mt-2 rounded-2xl border border-red-400/15 bg-red-500/10 px-3 py-2 text-xs text-red-200">{modelError}</div>}
        <Command className="model-command mt-3 border border-white/10 bg-white/[.035]">
          <CommandInput placeholder="Filter models..." />
          <CommandList className="max-h-[220px]">
            <CommandEmpty>{fetchingModels ? "Loading models..." : "No models loaded"}</CommandEmpty>
            <CommandGroup>
              {modelOptions.map((option) => (
                <CommandItem
                  key={option}
                  value={option}
                  onSelect={() => {
                    setDraft(option);
                    onModel(option);
                  }}
                  className="model-option"
                >
                  {option === model ? <Check className="h-3.5 w-3.5 text-zinc-100" /> : <Bot className="h-3.5 w-3.5 text-zinc-500" />}
                  <span className="min-w-0 truncate">{option}</span>
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}

function ReasoningControl({
  effort,
  onEffort
}: {
  effort: string;
  onEffort: (value: string) => void;
}) {
  const normalizedEffort = normalizeReasoningEffort(effort);
  return (
    <Popover>
      <PopoverTrigger asChild>
        <Button data-testid="reasoning-control" variant="subtle" size="sm" className="composer-token">
          <Brain className="h-3.5 w-3.5" />
          <span className="composer-token-label">Reasoning</span>
          <span className="composer-token-separator">·</span>
          <span className="composer-token-value">{normalizedEffort}</span>
        </Button>
      </PopoverTrigger>
      <PopoverContent className="composer-popover w-[240px] p-4" align="center">
        <div className="composer-popover-title">Reasoning</div>
        <Select value={normalizedEffort} onValueChange={(value) => onEffort(normalizeReasoningEffort(value))}>
          <SelectTrigger className="composer-select h-10 w-full"><SelectValue /></SelectTrigger>
          <SelectContent className="composer-select-content">
            {reasoningEfforts.map((item) => <SelectItem key={item} value={item} className="composer-select-item">{item}</SelectItem>)}
          </SelectContent>
        </Select>
      </PopoverContent>
    </Popover>
  );
}

function HitlControl({ hitl, onHitl }: { hitl: HITLConfig; onHitl: (value: HITLConfig) => void }) {
  const current = hitlChoiceFor(hitl);
  const Icon = current.mode === "off" ? ShieldAlert : ShieldCheck;
  const apply = (choice: (typeof hitlChoices)[number]) => {
    onHitl({
      ...hitl,
      enabled: choice.enabled,
      mode: choice.mode,
      timeoutSeconds: hitl.timeoutSeconds || 600
    });
  };

  return (
    <Popover>
      <PopoverTrigger asChild>
        <Button
          data-testid="hitl-control"
          variant="ghost"
          size="sm"
          className={cn("composer-hitl-token", `composer-hitl-token-${current.tone}`)}
        >
          <Icon className="h-3.5 w-3.5" />
          <span className="composer-token-label">HITL</span>
          <span className="composer-token-separator">·</span>
          <span className="composer-token-value">{current.label}</span>
          <ChevronDown className="h-3.5 w-3.5" />
        </Button>
      </PopoverTrigger>
      <PopoverContent className="composer-popover w-[320px] p-3" align="end">
        <div className="composer-popover-title px-2">HITL</div>
        <div className="space-y-1">
          {hitlChoices.map((choice) => {
            const selected = choice.mode === current.mode;
            const ChoiceIcon = choice.mode === "off" ? ShieldAlert : ShieldCheck;
            return (
              <button
                key={choice.mode}
                type="button"
                className={cn("hitl-choice", selected && "hitl-choice-selected", `hitl-choice-${choice.tone}`)}
                onClick={() => apply(choice)}
              >
                <ChoiceIcon className="h-3.5 w-3.5" />
                <span className="min-w-0 flex-1">
                  <span className="hitl-choice-title">{choice.label}</span>
                  <span className="hitl-choice-description">{choice.description}</span>
                </span>
                {selected && <Check className="h-3.5 w-3.5" />}
              </button>
            );
          })}
        </div>
        <div className="mt-3 px-2">
          <div className="mb-1 text-[11px] font-medium text-zinc-500">Approval timeout</div>
          <Input
            className="composer-field h-9 text-xs"
            type="number"
            value={hitl.timeoutSeconds || 600}
            onChange={(e) => onHitl({ ...hitl, timeoutSeconds: Number(e.target.value) })}
          />
        </div>
      </PopoverContent>
    </Popover>
  );
}

function TodoStatusDot({ status }: { status: PlanItem["status"] }) {
  if (status === "completed") {
    return (
      <span className="todo-status-dot todo-status-dot-completed">
        <Check className="h-3.5 w-3.5" />
      </span>
    );
  }
  return <span className={cn("todo-status-dot", status === "in_progress" && "todo-status-dot-active", status === "cancelled" && "todo-status-dot-cancelled")} />;
}

function EmptyLine({ text }: { text: string }) {
  return <div className="rounded-2xl border border-dashed border-white/10 px-3 py-3 text-xs text-zinc-500">{text}</div>;
}

function TraceRail({
  open,
  setOpen,
  events,
  tasks,
  details
}: {
  open: boolean;
  setOpen: (value: boolean) => void;
  events: RunEvent[];
  tasks: unknown[];
  details: unknown[];
}) {
  return (
    <div className="ml-3 flex w-12 shrink-0 flex-col items-center">
      <Sheet open={open} onOpenChange={setOpen}>
        <SheetTrigger asChild>
          <button data-testid="trace-rail" className="glass-panel flex h-full w-12 flex-col items-center justify-between rounded-[24px] py-4 text-zinc-400 transition hover:text-white">
            <ChevronLeft className="h-4 w-4" />
            <div className="-rotate-90 whitespace-nowrap text-xs tracking-normal">Trace</div>
            <Badge>{events.length}</Badge>
          </button>
        </SheetTrigger>
        <SheetContent>
          <SheetHeader>
            <SheetTitle>Trace</SheetTitle>
            <div className="text-xs text-zinc-500">Runtime events, active tasks, process details</div>
          </SheetHeader>
          <Tabs defaultValue="events" className="min-h-0 flex-1">
            <TabsList>
              <TabsTrigger value="events">Events</TabsTrigger>
              <TabsTrigger value="tasks">Tasks</TabsTrigger>
              <TabsTrigger value="details">Details</TabsTrigger>
            </TabsList>
            <TabsContent value="events" className="mt-3 h-[calc(100vh-9rem)]">
              <TraceList items={events.map((e) => ({ id: e.id, title: e.label, meta: e.type, detail: e.detail, time: e.time }))} />
            </TabsContent>
            <TabsContent value="tasks" className="mt-3 h-[calc(100vh-9rem)]">
              <TraceList items={tasks.map((t, i) => ({ id: String(i), title: compactText(t), meta: "task" }))} />
            </TabsContent>
            <TabsContent value="details" className="mt-3 h-[calc(100vh-9rem)]">
              <TraceList items={details.map((d, i) => ({ id: String(i), title: compactText(d), meta: "process" }))} />
            </TabsContent>
          </Tabs>
        </SheetContent>
      </Sheet>
    </div>
  );
}

function TraceList({ items }: { items: { id: string; title: string; meta?: string; detail?: string; time?: string }[] }) {
  return (
    <ScrollArea className="h-full pr-2">
      <div className="space-y-2">
        {items.length === 0 && <EmptyLine text="No trace events" />}
        {items.map((item) => (
          <div key={item.id} className="rounded-2xl border border-white/[.09] bg-white/6 px-3 py-2 text-xs">
            <div className="flex items-center justify-between gap-2">
              <span className="truncate text-zinc-200">{item.title}</span>
              <Badge>{item.meta}</Badge>
            </div>
            {item.detail && <div className="mt-1 line-clamp-3 text-zinc-500">{item.detail}</div>}
            {item.time && <div className="mt-1 text-[10px] text-zinc-600">{formatTime(item.time)}</div>}
          </div>
        ))}
      </div>
    </ScrollArea>
  );
}
