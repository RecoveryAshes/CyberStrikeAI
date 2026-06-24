import { apiFetch } from "./client";
import type {
  AgentTask,
  AppConfig,
  ChatRequest,
  ConversationDetail,
  ConversationSummary,
  HITLConfig,
  HITLPendingItem,
  ProcessDetail,
  Project,
  Role
} from "./types";

export const Api = {
  config: () => apiFetch<AppConfig>("/api/config"),
  updateConfig: (config: AppConfig) => apiFetch<AppConfig>("/api/config", { method: "PUT", body: JSON.stringify(config) }),
  listModels: () =>
    apiFetch<{ success: boolean; models?: string[]; error?: string; supported?: boolean }>("/api/config/list-models", {
      method: "POST"
    }),
  roles: () => apiFetch<{ roles: Role[] }>("/api/roles"),
  projects: () => apiFetch<{ projects: Project[]; total?: number }>("/api/projects?limit=500"),
  conversations: (search: string) =>
    apiFetch<{ conversations: ConversationSummary[]; total: number }>(
      `/api/conversations?limit=200&sort_by=updated_at${search ? `&search=${encodeURIComponent(search)}` : ""}`
    ),
  createConversation: (title = "New Chat") =>
    apiFetch<ConversationDetail>("/api/conversations", { method: "POST", body: JSON.stringify({ title }) }),
  conversation: (id: string) => apiFetch<ConversationDetail>(`/api/conversations/${id}?include_process_details=0`),
  renameConversation: (id: string, title: string) =>
    apiFetch<ConversationDetail>(`/api/conversations/${id}`, { method: "PUT", body: JSON.stringify({ title }) }),
  setConversationProject: (id: string, projectId: string) =>
    apiFetch<ConversationDetail>(`/api/conversations/${id}/project`, {
      method: "PUT",
      body: JSON.stringify({ projectId })
    }),
  deleteConversation: (id: string) => apiFetch<{ message: string }>(`/api/conversations/${id}`, { method: "DELETE" }),
  processDetails: (messageId: string) =>
    apiFetch<{ processDetails: ProcessDetail[] }>(`/api/messages/${messageId}/process-details`),
  cancel: (conversationId: string) =>
    apiFetch<{ status?: string; message?: string }>(`/api/agent-loop/cancel`, {
      method: "POST",
      body: JSON.stringify({ conversationId })
    }),
  tasks: () => apiFetch<{ tasks: AgentTask[] }>("/api/agent-loop/tasks"),
  hitlConfig: (conversationId: string) =>
    apiFetch<{ conversationId: string; hitl: HITLConfig; hitlGlobalToolWhitelist?: string[] }>(
      `/api/hitl/config/${conversationId}`
    ),
  saveHitlConfig: (conversationId: string, hitl: HITLConfig) =>
    apiFetch<{ ok: boolean }>("/api/hitl/config", {
      method: "PUT",
      body: JSON.stringify({ conversationId, ...hitl })
    }),
  hitlPending: (conversationId?: string) =>
    apiFetch<{ items: HITLPendingItem[] }>(
      `/api/hitl/pending?status=pending&pageSize=50${conversationId ? `&conversationId=${conversationId}` : ""}`
    ),
  decideHitl: (interruptId: string, decision: "approve" | "reject", comment?: string) =>
    apiFetch<{ ok: boolean }>("/api/hitl/decision", {
      method: "POST",
      body: JSON.stringify({ interruptId, decision, comment })
    }),
  streamPayload: (payload: ChatRequest) => payload
};
