import { compactText } from "../lib/utils";
import type { PlanItem, ProgressUpdate, ToolRun, TurnRun } from "./types";

export type RuntimeCellKind =
  | "plan"
  | "skill"
  | "mcp"
  | "knowledge"
  | "read"
  | "edit"
  | "search"
  | "command"
  | "tool"
  | "approval"
  | "error";

export type RuntimeCell = {
  id: string;
  kind: RuntimeCellKind;
  label: string;
  detail?: string;
  status?: ToolRun["status"] | "completed" | "running" | "failed" | "cancelled";
  time?: string;
};

export type RunActivityItem =
  | { id: string; kind: "progress"; time: string; update: ProgressUpdate }
  | { id: string; kind: "tool"; time: string; cell: RuntimeCell };

const skillPattern = /skill/i;
const mcpPattern = /mcp|::|chrome-devtools|devtools|browser/i;
const knowledgePattern =
  /knowledge|fact|search_project_facts|list_project_facts|get_project_fact|upsert_project_fact|restore_project_fact|deprecate_project_fact/i;
const builtinMcpToolNames = new Set([
  "record_vulnerability",
  "list_vulnerabilities",
  "get_vulnerability",
  "upsert_project_fact",
  "get_project_fact",
  "list_project_facts",
  "search_project_facts",
  "deprecate_project_fact",
  "restore_project_fact",
  "list_knowledge_risk_types",
  "search_knowledge_base",
  "analyze_image",
  "webshell_exec",
  "webshell_file_list",
  "webshell_file_read",
  "webshell_file_write",
  "manage_webshell_list",
  "manage_webshell_add",
  "manage_webshell_update",
  "manage_webshell_delete",
  "manage_webshell_test",
  "batch_task_list",
  "batch_task_get",
  "batch_task_create",
  "batch_task_start",
  "batch_task_rerun",
  "batch_task_pause",
  "batch_task_delete",
  "batch_task_update_metadata",
  "batch_task_update_schedule",
  "batch_task_schedule_enabled",
  "batch_task_add_task",
  "batch_task_update_task",
  "batch_task_remove_task",
  "c2_listener",
  "c2_session",
  "c2_task",
  "c2_task_manage",
  "c2_payload",
  "c2_event",
  "c2_profile",
  "c2_file"
]);
const systemProgressPatterns = [
  /^分析用户输入并准备运行上下文。?$/,
  /^正在请求模型分析任务。?$/,
  /^根据工具结果继续分析下一步。?$/,
  /^模型请求执行工具(?: .+)?。?$/,
  /^模型请求执行 \d+ 个工具：.+。?$/,
  /^开始执行工具 .+。?$/,
  /^工具 .+ 执行完成，已获得结果。?$/,
  /^工具 .+ 返回失败结果，已记录错误。?$/,
  /^工具 .+ 执行失败，已记录错误。?$/,
  /^工具结果已写回上下文，准备继续采样。?$/,
  /^Todo\/计划状态已更新。?$/,
  /^运行过程已完成，准备输出最终回复。?$/
];

export function assistantProgressUpdates(run: TurnRun): ProgressUpdate[] {
  const seen = new Set<string>();
  return run.progressUpdates.filter((update) => {
    const message = update.message.trim();
    if (!message || isSystemProgressMessage(message)) return false;
    const key = `${update.turnId || ""}:${message}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

export function isSystemProgressMessage(message: string) {
  const text = message.trim();
  return systemProgressPatterns.some((pattern) => pattern.test(text));
}

export function todoProgress(plan: PlanItem[]) {
  if (plan.length === 0) return null;
  const completed = plan.filter((item) => item.status === "completed").length;
  const currentIndex = plan.findIndex((item) => item.status === "in_progress");
  return {
    total: plan.length,
    completed,
    currentIndex: currentIndex >= 0 ? currentIndex : Math.min(completed, plan.length - 1),
    current: plan[currentIndex >= 0 ? currentIndex : Math.min(completed, plan.length - 1)]
  };
}

export function progressPreview(text: string, fallback = "运行进度更新") {
  const lines = text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean);
  const latest = lines[lines.length - 1] || text.trim() || fallback;
  return latest.length > 132 ? `${latest.slice(0, 132)}...` : latest;
}

export function runActivityCount(run: TurnRun) {
  return runActivityItems(run).length + run.plan.length;
}

export function runAuxiliaryCells(run: TurnRun): RuntimeCell[] {
  const cells: RuntimeCell[] = [];
  for (const tool of Object.values(run.tools)) {
    if (isPlanTool(tool)) continue;
    cells.push({
      id: `${run.id}-tool-${tool.id}`,
      kind: cellKindForTool(tool),
      label: runtimeCellLabelForTool(tool),
      detail: readableToolBody(tool),
      status: tool.status,
      time: tool.completedAt || tool.startedAt
    });
  }
  if (run.approvals.length > 0) {
    cells.push({
      id: `${run.id}-approvals`,
      kind: "approval",
      label: `${run.approvals.length} 个 HITL 审批`,
      detail: run.approvals.map((item) => compactText(item.payload || item.toolName || item.id)).join("\n"),
      status: run.approvals.some((item) => item.status === "pending") ? "running" : "completed",
      time: run.approvals[0]?.createdAt
    });
  }
  if (run.error) {
    cells.push({
      id: `${run.id}-error`,
      kind: "error",
      label: run.error,
      detail: run.error,
      status: "failed",
      time: run.completedAt
    });
  }
  return cells;
}

export function runActivityItems(run: TurnRun): RunActivityItem[] {
  const progressItems: RunActivityItem[] = assistantProgressUpdates(run).map((update) => ({
    id: `${run.id}-progress-${update.id}`,
    kind: "progress",
    time: update.time,
    update
  }));
  const toolItems: RunActivityItem[] = runAuxiliaryCells(run).map((cell) => ({
    id: cell.id,
    kind: "tool",
    time: cell.time || run.startedAt || "",
    cell
  }));
  return [...progressItems, ...toolItems].sort(compareActivityItems);
}

function compareActivityItems(left: RunActivityItem, right: RunActivityItem) {
  const leftTime = Date.parse(left.time);
  const rightTime = Date.parse(right.time);
  const leftValid = !Number.isNaN(leftTime);
  const rightValid = !Number.isNaN(rightTime);
  if (leftValid && rightValid && leftTime !== rightTime) return leftTime - rightTime;
  if (leftValid !== rightValid) return leftValid ? -1 : 1;
  return left.id.localeCompare(right.id);
}

export function auxiliarySummary(cells: RuntimeCell[]) {
  if (cells.length === 0) return "";
  if (cells.length === 1) return cells[0].label;
  const toolCount = cells.filter((cell) => cell.kind !== "plan" && cell.kind !== "approval" && cell.kind !== "error").length;
  const planCount = cells.filter((cell) => cell.kind === "plan").length;
  const approvalCount = cells.filter((cell) => cell.kind === "approval").length;
  const errorCount = cells.filter((cell) => cell.kind === "error").length;
  const parts = [
    toolCount ? `${toolCount} 个工具` : "",
    planCount ? `${planCount} 个计划` : "",
    approvalCount ? `${approvalCount} 个审批` : "",
    errorCount ? `${errorCount} 个错误` : ""
  ].filter(Boolean);
  return `运行详情 · ${parts.join(" · ") || `${cells.length} 项`}`;
}

function readableToolBody(tool: ToolRun) {
  const value = tool.output || tool.input;
  if (value == null || value === "") return "";
  if (typeof value === "string") {
    if (value === "[object Object]") return "";
    const parsed = parseJSON(value);
    if (parsed.ok) return readableStructuredToolBody(parsed.value);
    return value;
  }
  return readableStructuredToolBody(value);
}

function readableStructuredToolBody(value: unknown) {
  const extracted = extractToolText(value);
  if (extracted) return extracted;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return compactText(value, "");
  }
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

function stringField(value: Record<string, unknown>, key: string) {
  const item = value[key];
  return typeof item === "string" ? item : "";
}

function extractToolText(value: unknown): string {
  const obj = objectValue(value);
  if (!obj || Object.keys(obj).length === 0) return "";

  const nestedResult = stringField(obj, "result");
  if (nestedResult) {
    const parsed = parseJSON(nestedResult);
    if (parsed.ok) {
      const nested = extractToolText(parsed.value);
      if (nested) return nested;
    }
  }

  const stdout = stringField(obj, "stdout");
  const stderr = stringField(obj, "stderr");
  if (stdout || stderr) return [stdout, stderr ? `stderr:\n${stderr}` : ""].filter(Boolean).join("\n");

  return firstToolText(
    obj.output,
    obj.text,
    obj.content,
    obj.message,
    nestedResult
  );
}

function objectValue(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
}

function firstToolText(...values: unknown[]) {
  for (const value of values) {
    if (typeof value === "string" || typeof value === "number") {
      const text = String(value).trim();
      if (text && text !== "[object Object]" && text !== "<nil>") return text;
    }
  }
  return "";
}

function nestedToolText(input: Record<string, unknown>, key: string) {
  const nested = objectValue(input[key]);
  return firstToolText(nested.name, nested.id, nested.skill_name, nested.skillName, nested.tool, nested.tool_name, nested.toolName);
}

function skillDisplayName(tool: ToolRun) {
  const input = objectValue(tool.input);
  return firstToolText(
    input.name,
    input.skill_name,
    input.skillName,
    input.skill,
    input.id,
    input.skill_id,
    input.skillId,
    nestedToolText(input, "skill"),
    nestedToolText(input, "name"),
    tool.name === "skill" ? "" : tool.name
  );
}

function mcpTargetName(tool: ToolRun) {
  const input = objectValue(tool.input);
  const args = objectValue(input.arguments || input.args);
  const name = firstToolText(
    input.tool,
    input.name,
    input.tool_name,
    input.toolName,
    args.tool,
    args.name,
    args.tool_name,
    args.toolName,
    tool.name
  );
  if (!name) return "";
  if (name.includes("::")) return name;
  if (builtinMcpToolNames.has(name)) return `builtin::${name}`;
  return name;
}

function toolSearchText(tool: ToolRun) {
  return `${tool.name} ${compactText(tool.input, "")} ${compactText(tool.output, "")}`.toLowerCase();
}

function toolIdentityText(tool: ToolRun) {
  return `${tool.name} ${compactText(tool.input, "")}`.toLowerCase();
}

function isPlanTool(tool: ToolRun) {
  const name = tool.name.trim().toLowerCase();
  return name === "update_plan" || name === "todowrite";
}

function toolKind(tool: ToolRun): "read" | "edit" | "search" | "command" | "other" {
  const text = toolSearchText(tool);
  if (/write_file|write file|edit_file|apply_patch|patch|replace|create file|updated file/.test(text)) return "edit";
  if (/read_file|read file|view_image|open file|cat |sed -n|nl -ba/.test(text)) return "read";
  if (/search_file|search_code|grep|ripgrep|rg |find |search_project|searched/.test(text)) return "search";
  if (/shell_command|shell|command|exec|bash|zsh|ssh |rsync|npm |git /.test(text)) return "command";
  return "other";
}

function toolDisplayName(tool: ToolRun) {
  const kind = toolKind(tool);
  if (kind === "command") return "Shell";
  if (kind === "read") return "Read";
  if (kind === "edit") return "Edit";
  if (kind === "search") return "Search";
  return tool.name;
}

function cellKindForTool(tool: ToolRun): RuntimeCellKind {
  const text = toolIdentityText(tool);
  if (skillPattern.test(text)) return "skill";
  if (knowledgePattern.test(text)) return "knowledge";
  if (tool.name === "mcp_call" || builtinMcpToolNames.has(tool.name) || tool.name.includes("::")) return "mcp";
  if (mcpPattern.test(text)) return "mcp";
  const kind = toolKind(tool);
  if (kind === "read") return "read";
  if (kind === "edit") return "edit";
  if (kind === "search") return "search";
  if (kind === "command") return "command";
  return "tool";
}

function runtimeCellLabelForTool(tool: ToolRun) {
  const kind = cellKindForTool(tool);
  const name = tool.name && tool.name !== "tool" ? tool.name : toolDisplayName(tool);
  if (kind === "skill") return `已调用 Skill ${skillDisplayName(tool) || name}`;
  if (kind === "mcp") return `已调用 MCP ${mcpTargetName(tool) || name}`;
  if (kind === "knowledge") return "已查询知识库 1 次";
  if (kind === "read") return "已读取文件";
  if (kind === "edit") return "编辑了文件";
  if (kind === "search") return "已搜索代码";
  if (kind === "command") return "已运行命令";
  return `已运行 ${name}`;
}
