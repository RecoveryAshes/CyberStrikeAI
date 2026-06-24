export type AuthState = {
  token?: string;
  expiresAt?: string | number;
};

export type AppConfig = {
  openai?: {
    provider?: string;
    base_url?: string;
    baseURL?: string;
    api_key?: string;
    apiKey?: string;
    model?: string;
    reasoning?: {
      mode?: string;
      effort?: string;
    };
  };
  agent_runtime?: {
    enabled?: boolean;
  };
  [key: string]: unknown;
};

export type Role = {
  name: string;
  description?: string;
  prompt?: string;
  [key: string]: unknown;
};

export type Project = {
  id: string;
  name: string;
  description?: string;
  status?: string;
  pinned?: boolean;
  created_at?: string;
  updated_at?: string;
};

export type ConversationSummary = {
  id: string;
  title: string;
  projectId?: string;
  pinned?: boolean;
  createdAt?: string;
  updatedAt?: string;
};

export type ConversationMessage = {
  id: string;
  conversationId: string;
  role: "user" | "assistant" | "system" | string;
  content: string;
  reasoningContent?: string;
  processDetails?: ProcessDetail[];
  createdAt?: string;
  updatedAt?: string;
};

export type ConversationDetail = ConversationSummary & {
  messages?: ConversationMessage[];
};

export type ProcessDetail = {
  id: string;
  messageId?: string;
  conversationId?: string;
  eventType: string;
  message?: string;
  data?: unknown;
  createdAt?: string;
};

export type HITLConfig = {
  enabled: boolean;
  mode?: string;
  sensitiveTools?: string[];
  timeoutSeconds?: number;
};

export type HITLPendingItem = {
  id: string;
  conversationId: string;
  messageId?: string;
  mode?: string;
  toolName?: string;
  toolCallId?: string;
  payload?: string;
  status?: string;
  decision?: string;
  comment?: string;
  createdAt?: string;
  decidedAt?: string | null;
};

export type AgentTask = {
  id?: string;
  taskId?: string;
  conversationId?: string;
  status?: string;
  title?: string;
  message?: string;
  createdAt?: string;
  updatedAt?: string;
  [key: string]: unknown;
};

export type ChatRequest = {
  message: string;
  conversationId?: string;
  projectId?: string;
  role?: string;
  hitl?: HITLConfig;
  reasoning?: {
    mode?: string;
    effort?: string;
  };
  background?: boolean;
};

export type SSEEnvelope = {
  type?: string;
  message?: string;
  data?: Record<string, unknown>;
  [key: string]: unknown;
};
