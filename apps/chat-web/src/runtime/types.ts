import type { AgentTask, ConversationMessage, HITLPendingItem, ProcessDetail } from "../api/types";

export type PlanItemStatus = "pending" | "in_progress" | "completed" | "cancelled";

export type PlanItem = {
  id: string;
  content: string;
  status: PlanItemStatus;
};

export type ToolRun = {
  id: string;
  name: string;
  status: "running" | "completed" | "failed";
  input?: unknown;
  output?: string;
  startedAt?: string;
  completedAt?: string;
};

export type ApprovalRequest = HITLPendingItem & {
  status?: string;
};

export type RunEvent = {
  id: string;
  type: string;
  label: string;
  detail?: string;
  time: string;
  raw?: unknown;
};

export type ProgressUpdate = {
  id: string;
  message: string;
  time: string;
  turnId?: string;
  assistantMessageId?: string;
};

export type TurnRun = {
  id: string;
  origin?: "stream" | "task";
  conversationId?: string;
  assistantMessageId?: string;
  userMessage?: string;
  assistantText: string;
  reasoningText: string;
  progressUpdates: ProgressUpdate[];
  status: "idle" | "running" | "awaiting_approval" | "completed" | "cancelled" | "error";
  error?: string;
  plan: PlanItem[];
  tools: Record<string, ToolRun>;
  approvals: ApprovalRequest[];
  events: RunEvent[];
  startedAt?: string;
  completedAt?: string;
};

export type RuntimeState = {
  activeRun: TurnRun | null;
  runs: TurnRun[];
  activeTasks: AgentTask[];
  processDetails: ProcessDetail[];
  draftAssistantMessage?: ConversationMessage;
};

type ScopedRunAction = {
  runId?: string;
};

export type RuntimeAction =
  | ({ type: "start"; conversationId?: string; message: string; id?: string; startedAt?: string } & ScopedRunAction)
  | { type: "ensure_run"; conversationId: string; message?: string; startedAt?: string; origin?: TurnRun["origin"] }
  | ({ type: "adopt_task" } & ScopedRunAction)
  | ({ type: "event"; event: RunEvent; patch?: Partial<TurnRun> } & ScopedRunAction)
  | ({ type: "assistant_delta"; delta: string; accumulated?: string } & ScopedRunAction)
  | ({ type: "reasoning_delta"; delta: string; accumulated?: string } & ScopedRunAction)
  | ({ type: "progress_update"; update: ProgressUpdate } & ScopedRunAction)
  | ({ type: "plan"; items: PlanItem[] } & ScopedRunAction)
  | ({ type: "tool"; tool: ToolRun } & ScopedRunAction)
  | ({ type: "tool_delta"; toolId: string; delta: string } & ScopedRunAction)
  | ({ type: "approval"; approval: ApprovalRequest } & ScopedRunAction)
  | ({ type: "finish"; status: TurnRun["status"]; error?: string } & ScopedRunAction)
  | { type: "tasks"; tasks: AgentTask[] }
  | { type: "hydrate_tasks"; tasks: AgentTask[] }
  | { type: "process_details"; details: ProcessDetail[] }
  | ({ type: "reset_active" } & ScopedRunAction)
  | { type: "reset_conversation"; conversationId?: string }
  | ({ type: "clear_draft" } & ScopedRunAction);
