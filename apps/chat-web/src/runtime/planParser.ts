import type { PlanItem, PlanItemStatus } from "./types";

const statusMap: Record<string, PlanItemStatus> = {
  pending: "pending",
  in_progress: "in_progress",
  inprogress: "in_progress",
  running: "in_progress",
  active: "in_progress",
  completed: "completed",
  complete: "completed",
  done: "completed",
  cancelled: "cancelled",
  canceled: "cancelled"
};

export function parseMarkdownPlanItems(text: string, idPrefix = "plan"): PlanItem[] {
  const items: PlanItem[] = [];
  for (const [index, line] of text.split(/\r?\n/).entries()) {
    const match = line.match(/^\s*(?:[-*]|\d+[.)])\s*\[\s*([^\]]+?)\s*\]\s*(.+?)\s*$/);
    if (!match) continue;
    const status = statusMap[match[1].trim().toLowerCase()];
    const content = match[2].trim();
    if (!status || !content) continue;
    items.push({
      id: `${idPrefix}-${index}-${content.slice(0, 24)}`,
      content,
      status
    });
  }
  return items;
}

export function normalizePlanItems(raw: unknown, idPrefix = "plan"): PlanItem[] {
  const maybeItems =
    raw && typeof raw === "object" && !Array.isArray(raw) && "items" in raw ? (raw as { items?: unknown }).items : raw;
  if (typeof maybeItems === "string") return parseMarkdownPlanItems(maybeItems, idPrefix);
  if (!Array.isArray(maybeItems)) return [];
  return maybeItems.map((item, index) => {
    if (item && typeof item === "object") {
      const obj = item as Record<string, unknown>;
      return {
        id: String(obj.id || obj.content || obj.task || `${idPrefix}-${index}`),
        content: String(obj.content || obj.task || obj.step || obj.text || obj.title || `Step ${index + 1}`),
        status: String(obj.status || "pending") as PlanItemStatus
      };
    }
    return {
      id: `${idPrefix}-${index}`,
      content: String(item || `Step ${index + 1}`),
      status: "pending"
    };
  });
}
