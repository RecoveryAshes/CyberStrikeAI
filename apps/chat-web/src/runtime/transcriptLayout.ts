import type { ConversationMessage } from "../api/types";
import type { TurnRun } from "./types";

function normalize(text: string | undefined) {
  return (text || "").replace(/\s+/g, " ").trim();
}

function textLooksRelated(left: string | undefined, right: string | undefined) {
  const a = normalize(left);
  const b = normalize(right);
  if (!a || !b) return false;
  return a === b || a.startsWith(b) || b.startsWith(a);
}

function assistantIdsFromRun(run: TurnRun) {
  return new Set(
    [
      run.assistantMessageId,
      ...run.progressUpdates.map((update) => update.assistantMessageId)
    ].filter((id): id is string => Boolean(id))
  );
}

export function lastUserIndex(messages: Pick<ConversationMessage, "role">[]) {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    if (messages[index].role === "user") return index;
  }
  return -1;
}

export function activeRunAssistantMessageId(
  messages: Pick<ConversationMessage, "id" | "role" | "content">[],
  activeRun: TurnRun | null,
  runsByMessageId: Record<string, TurnRun>
) {
  if (!activeRun) return "";
  const userIndex = lastUserIndex(messages);
  if (userIndex < 0) return "";
  const assistantMessages = messages.slice(userIndex + 1).filter((message) => message.role === "assistant");
  if (assistantMessages.length === 0) return "";

  const knownAssistantIds = assistantIdsFromRun(activeRun);
  const explicit = assistantMessages.find((message) => knownAssistantIds.has(message.id));
  if (explicit) return explicit.id;

  const byContent = assistantMessages.find((message) => textLooksRelated(activeRun.assistantText, message.content));
  if (byContent) return byContent.id;

  const byRestoredRun = assistantMessages.find((message) => runsByMessageId[message.id]);
  if (byRestoredRun) return byRestoredRun.id;

  return assistantMessages[assistantMessages.length - 1].id;
}
