import type { AuthState } from "./types";

const API_BASE = import.meta.env.VITE_API_BASE_URL || "";

export class ApiError extends Error {
  status: number;
  body?: unknown;

  constructor(message: string, status: number, body?: unknown) {
    super(message);
    this.status = status;
    this.body = body;
  }
}

export function getAuthState(): AuthState | null {
  try {
    const raw = localStorage.getItem("cyberstrike-auth");
    if (!raw) return null;
    const parsed = JSON.parse(raw) as AuthState & { expires_at?: string | number };
    return {
      token: parsed.token,
      expiresAt: parsed.expiresAt ?? parsed.expires_at
    };
  } catch {
    return null;
  }
}

export function getAuthToken() {
  const auth = getAuthState();
  if (!auth?.token) return "";
  if (auth.expiresAt) {
    const t = new Date(auth.expiresAt).getTime();
    if (!Number.isNaN(t) && t < Date.now()) return "";
  }
  return auth.token;
}

export function authHeaders(extra?: HeadersInit): HeadersInit {
  const token = getAuthToken();
  return {
    ...(extra || {}),
    ...(token ? { Authorization: `Bearer ${token}` } : {})
  };
}

async function parseBody(res: Response) {
  const text = await res.text();
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

export async function apiFetch<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = authHeaders({
    Accept: "application/json",
    ...(init.body ? { "Content-Type": "application/json" } : {}),
    ...init.headers
  });
  const res = await fetch(`${API_BASE}${path}`, { ...init, headers });
  const body = await parseBody(res);
  if (!res.ok) {
    const message =
      typeof body === "object" && body && "error" in body
        ? String((body as { error: unknown }).error)
        : `HTTP ${res.status}`;
    throw new ApiError(message, res.status, body);
  }
  return body as T;
}

export async function apiStream(path: string, payload: unknown, onChunk: (line: string) => void, signal?: AbortSignal) {
  const res = await fetch(`${API_BASE}${path}`, {
    method: "POST",
    headers: authHeaders({
      Accept: "text/event-stream",
      "Content-Type": "application/json"
    }),
    body: JSON.stringify(payload),
    signal
  });

  if (!res.ok || !res.body) {
    const body = await parseBody(res);
    const message =
      typeof body === "object" && body && "error" in body
        ? String((body as { error: unknown }).error)
        : `HTTP ${res.status}`;
    throw new ApiError(message, res.status, body);
  }

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split(/\r?\n/);
    buffer = lines.pop() || "";
    for (const line of lines) onChunk(line);
  }
  if (buffer) onChunk(buffer);
}
