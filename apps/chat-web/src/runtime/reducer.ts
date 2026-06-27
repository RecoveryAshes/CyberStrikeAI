import type { RuntimeAction, RuntimeState, ToolRun, TurnRun } from "./types";

export const initialRuntimeState: RuntimeState = {
  activeRun: null,
  runs: [],
  activeTasks: [],
  processDetails: []
};

const MAX_TRACKED_RUNS = 500;

function taskRunId(conversationId: string, startedAt?: string) {
  const started = startedAt ? Date.parse(startedAt) : Number.NaN;
  return `task-${conversationId}-${Number.isNaN(started) ? Date.now() : started}`;
}

function taskString(value: unknown) {
  return typeof value === "string" ? value : "";
}

function sortRuns(runs: TurnRun[]) {
  return [...runs].sort((a, b) => {
    const aTime = a.startedAt ? Date.parse(a.startedAt) : 0;
    const bTime = b.startedAt ? Date.parse(b.startedAt) : 0;
    return (Number.isNaN(bTime) ? 0 : bTime) - (Number.isNaN(aTime) ? 0 : aTime);
  });
}

function runHasActivity(run: TurnRun) {
  return Boolean(
    run.assistantText ||
      run.reasoningText ||
      run.progressUpdates.length ||
      run.plan.length ||
      Object.keys(run.tools).length ||
      run.approvals.length ||
      run.events.length
  );
}

function updateRun(state: RuntimeState, runId: string | undefined, update: (run: TurnRun) => TurnRun): RuntimeState {
  const targetId = runId || state.activeRun?.id;
  const targetRun = targetId ? state.runs.find((run) => run.id === targetId) : state.activeRun;
  if (!targetRun) return state;
  const updatedRun = update(targetRun);
  const runs = state.runs.map((run) => (run.id === updatedRun.id ? updatedRun : run));
  const activeRun = state.activeRun?.id === updatedRun.id ? updatedRun : state.activeRun;
  return {
    ...state,
    activeRun,
    runs
  };
}

function runIdForConversation(state: RuntimeState, conversationId?: string) {
  if (!conversationId) return undefined;
  return state.runs.find((run) => run.conversationId === conversationId && (run.status === "running" || run.status === "awaiting_approval"))?.id ||
    state.runs.find((run) => run.conversationId === conversationId)?.id;
}

function mergeTool(tools: Record<string, ToolRun>, tool: ToolRun) {
  const prev = tools[tool.id];
  return {
    ...tools,
    [tool.id]: {
      ...prev,
      ...tool,
      input: tool.input === undefined ? prev?.input : tool.input,
      output: tool.output === undefined || tool.output === "" ? prev?.output : tool.output,
      startedAt: tool.startedAt || prev?.startedAt,
      completedAt: tool.completedAt || prev?.completedAt
    }
  };
}

export function runtimeReducer(state: RuntimeState, action: RuntimeAction): RuntimeState {
  switch (action.type) {
    case "start": {
      const run: TurnRun = {
        id: action.id || `run-${Date.now()}`,
        origin: "stream",
        conversationId: action.conversationId,
        userMessage: action.message,
        assistantText: "",
        reasoningText: "",
        progressUpdates: [],
        status: "running",
        plan: [],
        tools: {},
        approvals: [],
        events: [],
        startedAt: action.startedAt || new Date().toISOString()
      };
      return { ...state, activeRun: run, runs: sortRuns([run, ...state.runs]).slice(0, MAX_TRACKED_RUNS) };
    }
    case "ensure_run": {
      const existing = state.runs.find(
        (run) => run.conversationId === action.conversationId && (run.status === "running" || run.status === "awaiting_approval")
      );
      if (existing) return state;
      const run: TurnRun = {
        id: taskRunId(action.conversationId, action.startedAt),
        origin: action.origin || "task",
        conversationId: action.conversationId,
        userMessage: action.message,
        assistantText: "",
        reasoningText: "",
        progressUpdates: [],
        status: "running",
        plan: [],
        tools: {},
        approvals: [],
        events: [],
        startedAt: action.startedAt || new Date().toISOString()
      };
      return { ...state, activeRun: run, runs: sortRuns([run, ...state.runs]).slice(0, MAX_TRACKED_RUNS) };
    }
    case "adopt_task":
      return updateRun(state, action.runId, (run) => ({ ...run, origin: "task" }));
    case "hydrate_tasks": {
      const activeConversationIds = new Set(
        action.tasks
          .map((task) => taskString(task.conversationId).trim())
          .filter((id): id is string => Boolean(id))
      );
      const existingRunningConversationIds = new Set(
        state.runs
          .filter((run) => run.status === "running" || run.status === "awaiting_approval")
          .map((run) => run.conversationId)
          .filter(Boolean)
      );
      const existingActivityConversationIds = new Set(
        state.runs
          .filter(runHasActivity)
          .map((run) => run.conversationId)
          .filter(Boolean)
      );
      const hydratedRuns = action.tasks
        .map((task) => ({
          task,
          conversationId: taskString(task.conversationId).trim(),
          startedAt: taskString(task.startedAt),
          message: taskString(task.message)
        }))
        .filter(({ conversationId }) => conversationId && !existingRunningConversationIds.has(conversationId) && !existingActivityConversationIds.has(conversationId))
        .map<TurnRun>(({ task, conversationId, startedAt, message }) => ({
          id: taskRunId(conversationId, startedAt),
          origin: "task",
          conversationId,
          userMessage: message,
          assistantText: "",
          reasoningText: "",
          progressUpdates: [],
          status: task.status === "cancelling" ? "cancelled" : "running",
          plan: [],
          tools: {},
          approvals: [],
          events: [],
          startedAt: startedAt || new Date().toISOString()
        }));
      const runs = sortRuns([...hydratedRuns, ...state.runs])
        .map((run) => {
          if (run.origin !== "task") return run;
          if (run.conversationId && activeConversationIds.has(run.conversationId)) {
            return run.status === "running" || run.status === "awaiting_approval" ? run : { ...run, status: "running" as const };
          }
          if (run.status === "running" || run.status === "awaiting_approval") {
            return { ...run, status: "completed" as const, completedAt: run.completedAt || new Date().toISOString() };
          }
          return run;
        })
        .slice(0, MAX_TRACKED_RUNS);
      const activeRun = state.activeRun ? runs.find((run) => run.id === state.activeRun?.id) || null : hydratedRuns[0] || null;
      return { ...state, activeRun, runs };
    }
    case "event":
      return updateRun(state, action.runId || runIdForConversation(state, action.patch?.conversationId), (run) => ({
        ...run,
        ...action.patch,
        events: [action.event, ...run.events].slice(0, 500)
      }));
    case "assistant_delta":
      return updateRun(state, action.runId, (run) => ({
        ...run,
        assistantText: action.accumulated ?? `${run.assistantText}${action.delta}`
      }));
    case "reasoning_delta":
      return updateRun(state, action.runId, (run) => ({
        ...run,
        reasoningText: action.accumulated ?? `${run.reasoningText}${action.delta}`
      }));
    case "progress_update":
      return updateRun(state, action.runId, (run) => ({
        ...run,
        progressUpdates: [
          ...run.progressUpdates.filter((item) => item.id !== action.update.id),
          action.update
        ].slice(-100)
      }));
    case "plan":
      if (action.conversationId && !action.runId) {
        const existingRun = state.runs.find((run) => run.conversationId === action.conversationId);
        if (existingRun) {
          return updateRun(state, existingRun.id, (run) => ({
            ...run,
            plan: action.items,
            status: action.status || run.status
          }));
        }
        if (action.items.length === 0) return state;
        const run: TurnRun = {
          id: taskRunId(action.conversationId),
          origin: "task",
          conversationId: action.conversationId,
          assistantText: "",
          reasoningText: "",
          progressUpdates: [],
          status: action.status || "completed",
          plan: action.items,
          tools: {},
          approvals: [],
          events: [],
          startedAt: new Date().toISOString(),
          completedAt: action.status === "running" || action.status === "awaiting_approval" ? undefined : new Date().toISOString()
        };
        return { ...state, activeRun: state.activeRun, runs: sortRuns([run, ...state.runs]).slice(0, MAX_TRACKED_RUNS) };
      }
      return updateRun(state, action.runId, (run) => ({ ...run, plan: action.items }));
    case "tool":
      return updateRun(state, action.runId, (run) => ({ ...run, tools: mergeTool(run.tools, action.tool) }));
    case "tool_delta":
      return updateRun(state, action.runId, (run) => {
        const prev = run.tools[action.toolId];
        if (!prev) return run;
        return {
          ...run,
          tools: {
            ...run.tools,
            [action.toolId]: {
              ...prev,
              output: `${prev.output || ""}${action.delta}`
            }
          }
        };
      });
    case "approval":
      return updateRun(state, action.runId, (run) => ({
        ...run,
        status: "awaiting_approval",
        approvals: [action.approval, ...run.approvals.filter((item) => item.id !== action.approval.id)]
      }));
    case "finish":
      return updateRun(state, action.runId, (run) => ({
        ...run,
        status: action.status,
        error: action.error,
        completedAt: new Date().toISOString()
      }));
    case "tasks":
      return { ...state, activeTasks: action.tasks };
    case "process_details":
      return { ...state, processDetails: action.details };
    case "reset_active":
      return {
        ...state,
        activeRun: action.runId ? (state.activeRun?.id === action.runId ? null : state.activeRun) : null,
        runs: action.runId ? state.runs.filter((run) => run.id !== action.runId) : state.runs
      };
    case "reset_conversation":
      return {
        ...state,
        processDetails: []
      };
    case "clear_draft":
      return {
        ...state,
        activeRun: action.runId ? (state.activeRun?.id === action.runId ? null : state.activeRun) : null,
        runs: action.runId ? state.runs.filter((run) => run.id !== action.runId) : state.runs
      };
    default:
      return state;
  }
}
