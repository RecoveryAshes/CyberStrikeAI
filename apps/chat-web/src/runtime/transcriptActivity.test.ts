import assert from "node:assert/strict";
import test from "node:test";

import { adaptSSE } from "./eventAdapter";
import { initialRuntimeState, runtimeReducer } from "./reducer";
import { assistantProgressUpdates, runActivityCount, runActivityItems, runAuxiliaryCells, todoProgress } from "./transcriptActivity";
import { activeRunAssistantMessageId, lastUserIndex } from "./transcriptLayout";
import type { RuntimeAction, RuntimeState } from "./types";

function applyEvents(state: RuntimeState, events: unknown[]) {
  let next = state;
  for (const event of events) {
    for (const action of adaptSSE(event as never)) {
      next = runtimeReducer(next, action);
    }
  }
  return next;
}

function startRun(id: string, message: string, state: RuntimeState = initialRuntimeState, conversationId = "conv-1") {
  return runtimeReducer(state, {
    type: "start",
    id,
    conversationId,
    message,
    startedAt: "2026-06-23T09:00:00.000Z"
  });
}

function runningRunIdForConversation(state: RuntimeState, conversationId: string) {
  return state.runs.find((run) => run.conversationId === conversationId && (run.status === "running" || run.status === "awaiting_approval"))?.id;
}

function applyGlobalTaskEvent(state: RuntimeState, conversationId: string, event: unknown) {
  let next = state;
  let runId = runningRunIdForConversation(next, conversationId);
  if (!runId) {
    next = runtimeReducer(next, {
      type: "ensure_run",
      conversationId,
      message: `message for ${conversationId}`,
      startedAt: "2026-06-23T09:00:00.000Z",
      origin: "task"
    });
    runId = runningRunIdForConversation(next, conversationId);
  }
  for (const action of adaptSSE(event as never)) {
    next = runtimeReducer(next, { ...action, runId } as RuntimeAction);
  }
  return next;
}

test("assistant progress updates appear while the run is streaming", () => {
  let state = startRun("run-1", "inspect processes");

  state = applyEvents(state, [
    {
      type: "assistant_progress_update",
      message: "正在检查本机进程。",
      data: {
        runtimeEventType: "assistant_progress_update",
        assistantMessageId: "assistant-1",
        turnId: "turn-1",
        runtimeTrace: {
          event: "assistant_progress_update",
          message: "正在检查本机进程。",
          turnId: "turn-1"
        }
      }
    }
  ]);

  assert.equal(state.activeRun?.status, "running");
  assert.deepEqual(
    assistantProgressUpdates(state.activeRun!).map((item) => item.message),
    ["正在检查本机进程。"]
  );
  assert.equal(state.activeRun?.progressUpdates[0].assistantMessageId, "assistant-1");
});

test("assistant message id from SSE is stored on the active run", () => {
  let state = startRun("run-1", "inspect processes");

  state = applyEvents(state, [
    {
      type: "response_delta",
      message: "CPU 最大的是 python3。",
      data: {
        assistantMessageId: "assistant-1",
        runtimeEventType: "assistant_delta",
        runtimeTrace: {
          event: "assistant_delta",
          delta: "CPU 最大的是 python3。",
          accumulated: "CPU 最大的是 python3。"
        }
      }
    }
  ]);

  assert.equal(state.activeRun?.assistantMessageId, "assistant-1");
});


test("final answer does not clear assistant progress updates", () => {
  let state = startRun("run-1", "inspect processes");

  state = applyEvents(state, [
    {
      type: "assistant_progress_update",
      message: "正在检查本机进程。",
      data: {
        runtimeEventType: "assistant_progress_update",
        assistantMessageId: "assistant-1",
        turnId: "turn-1"
      }
    },
    {
      type: "response_delta",
      message: "CPU 最大的是 python3。",
      data: {
        runtimeEventType: "assistant_delta",
        runtimeTrace: {
          event: "assistant_delta",
          delta: "CPU 最大的是 python3。",
          accumulated: "CPU 最大的是 python3。"
        }
      }
    },
    { type: "done", data: { conversationId: "conv-1" } }
  ]);

  assert.equal(state.activeRun?.status, "completed");
  assert.equal(state.activeRun?.assistantText, "CPU 最大的是 python3。");
  assert.deepEqual(
    assistantProgressUpdates(state.activeRun!).map((item) => item.message),
    ["正在检查本机进程。"]
  );
});

test("turn completed response fills assistant text when deltas were missed", () => {
  let state = startRun("run-1", "short answer");

  state = applyEvents(state, [
    {
      type: "turn_completed",
      message: "alpha：首位\nbeta：测试版\ngamma：伽马",
      data: {
        conversationId: "conv-1",
        runtimeEventType: "turn_completed",
        runtimeTrace: {
          type: "turn_completed",
          response: "alpha：首位\nbeta：测试版\ngamma：伽马"
        }
      }
    }
  ]);

  assert.equal(state.activeRun?.status, "completed");
  assert.equal(state.activeRun?.assistantText, "alpha：首位\nbeta：测试版\ngamma：伽马");
});

test("a second user turn does not mutate the first assistant message progress history", () => {
  let state = startRun("run-1", "first");
  state = applyEvents(state, [
    {
      type: "assistant_progress_update",
      message: "第一轮正在处理。",
      data: { runtimeEventType: "assistant_progress_update", assistantMessageId: "assistant-1", turnId: "turn-1" }
    },
    { type: "response", message: "第一轮最终回复。", data: { conversationId: "conv-1" } }
  ]);
  const firstRun = state.activeRun!;
  state = runtimeReducer(state, { type: "clear_draft" });

  state = startRun("run-2", "second", state);
  state = applyEvents(state, [
    {
      type: "assistant_progress_update",
      message: "第二轮正在处理。",
      data: { runtimeEventType: "assistant_progress_update", assistantMessageId: "assistant-2", turnId: "turn-2" }
    }
  ]);

  assert.deepEqual(
    assistantProgressUpdates(firstRun).map((item) => item.message),
    ["第一轮正在处理。"]
  );
  assert.deepEqual(
    assistantProgressUpdates(state.activeRun!).map((item) => item.message),
    ["第二轮正在处理。"]
  );
});

test("background SSE updates the scoped run after another run becomes active", () => {
  let state = startRun("run-1", "first", initialRuntimeState, "conv-1");
  state = startRun("run-2", "second", state, "conv-2");

  state = runtimeReducer(state, {
    type: "assistant_delta",
    runId: "run-1",
    delta: "first answer",
    accumulated: "first answer"
  });
  state = runtimeReducer(state, {
    type: "assistant_delta",
    runId: "run-2",
    delta: "second answer",
    accumulated: "second answer"
  });

  assert.equal(state.runs.find((run) => run.id === "run-1")?.assistantText, "first answer");
  assert.equal(state.runs.find((run) => run.id === "run-2")?.assistantText, "second answer");
  assert.equal(state.activeRun?.id, "run-2");
});

test("active task hydration creates independent background runs", () => {
  let state = runtimeReducer(initialRuntimeState, {
    type: "hydrate_tasks",
    tasks: [
      {
        conversationId: "conv-1",
        message: "first",
        status: "running",
        startedAt: "2026-06-23T09:00:00.000Z"
      },
      {
        conversationId: "conv-2",
        message: "second",
        status: "running",
        startedAt: "2026-06-23T09:01:00.000Z"
      }
    ]
  });

  assert.equal(state.runs.length, 2);
  assert.equal(state.runs.find((run) => run.conversationId === "conv-1")?.userMessage, "first");
  assert.equal(state.runs.find((run) => run.conversationId === "conv-2")?.userMessage, "second");

  state = runtimeReducer(state, {
    type: "assistant_delta",
    runId: state.runs.find((run) => run.conversationId === "conv-1")?.id,
    delta: "first answer",
    accumulated: "first answer"
  });
  state = runtimeReducer(state, {
    type: "assistant_delta",
    runId: state.runs.find((run) => run.conversationId === "conv-2")?.id,
    delta: "second answer",
    accumulated: "second answer"
  });

  assert.equal(state.runs.find((run) => run.conversationId === "conv-1")?.assistantText, "first answer");
  assert.equal(state.runs.find((run) => run.conversationId === "conv-2")?.assistantText, "second answer");
});

test("global task events create a scoped background run before applying the first event", () => {
  const state = applyGlobalTaskEvent(initialRuntimeState, "conv-1", {
    type: "assistant_progress_update",
    message: "后台会话正在检查进程。",
    data: {
      conversationId: "conv-1",
      runtimeEventType: "assistant_progress_update",
      assistantMessageId: "assistant-1",
      turnId: "turn-1"
    }
  });

  assert.equal(state.runs.length, 1);
  assert.equal(state.runs[0].conversationId, "conv-1");
  assert.deepEqual(
    assistantProgressUpdates(state.runs[0]).map((item) => item.message),
    ["后台会话正在检查进程。"]
  );
});

test("interleaved global task events stay scoped to their conversations", () => {
  let state = initialRuntimeState;
  state = applyGlobalTaskEvent(state, "conv-a", {
    type: "response_delta",
    message: "A1",
    data: { conversationId: "conv-a", runtimeEventType: "assistant_delta", accumulated: "A1" }
  });
  state = applyGlobalTaskEvent(state, "conv-b", {
    type: "response_delta",
    message: "B1",
    data: { conversationId: "conv-b", runtimeEventType: "assistant_delta", accumulated: "B1" }
  });
  state = applyGlobalTaskEvent(state, "conv-a", {
    type: "response_delta",
    message: "A2",
    data: { conversationId: "conv-a", runtimeEventType: "assistant_delta", accumulated: "A1A2" }
  });

  assert.equal(state.runs.find((run) => run.conversationId === "conv-a")?.assistantText, "A1A2");
  assert.equal(state.runs.find((run) => run.conversationId === "conv-b")?.assistantText, "B1");
});

test("task hydration completes only task-origin runs that disappeared from active tasks", () => {
  let state = runtimeReducer(initialRuntimeState, {
    type: "start",
    id: "stream-run",
    conversationId: "conv-stream",
    message: "stream",
    startedAt: "2026-06-23T09:00:00.000Z"
  });
  state = runtimeReducer(state, {
    type: "hydrate_tasks",
    tasks: [
      {
        conversationId: "conv-task",
        message: "task",
        status: "running",
        startedAt: "2026-06-23T09:01:00.000Z"
      }
    ]
  });
  const taskRunId = state.runs.find((run) => run.conversationId === "conv-task")?.id;

  state = runtimeReducer(state, { type: "hydrate_tasks", tasks: [] });

  assert.equal(state.runs.find((run) => run.id === "stream-run")?.status, "running");
  assert.equal(state.runs.find((run) => run.id === taskRunId)?.status, "completed");
});

test("task hydration does not create an empty duplicate over a run with activity", () => {
  let state = runtimeReducer(initialRuntimeState, {
    type: "start",
    id: "run-1",
    conversationId: "conv-1",
    message: "run command",
    startedAt: "2026-06-23T09:00:00.000Z"
  });
  state = runtimeReducer(state, {
    type: "tool",
    runId: "run-1",
    tool: {
      id: "call-1",
      name: "execute",
      status: "running",
      output: "STREAM_STEP_1\n"
    }
  });

  state = runtimeReducer(state, {
    type: "hydrate_tasks",
    tasks: [
      {
        conversationId: "conv-1",
        message: "run command",
        status: "running",
        startedAt: "2026-06-23T09:00:01.000Z"
      }
    ]
  });

  assert.equal(state.runs.filter((run) => run.conversationId === "conv-1").length, 1);
  assert.equal(state.runs[0].tools["call-1"]?.output, "STREAM_STEP_1\n");
});

test("adopted stream runs are completed by task hydration fallback", () => {
  let state = runtimeReducer(initialRuntimeState, {
    type: "start",
    id: "stream-run",
    conversationId: "conv-1",
    message: "stream",
    startedAt: "2026-06-23T09:00:00.000Z"
  });
  state = runtimeReducer(state, { type: "adopt_task", runId: "stream-run" });
  state = runtimeReducer(state, { type: "hydrate_tasks", tasks: [] });

  assert.equal(state.runs.find((run) => run.id === "stream-run")?.origin, "task");
  assert.equal(state.runs.find((run) => run.id === "stream-run")?.status, "completed");
});

test("plain stream runs are not completed by unrelated task polling", () => {
  let state = runtimeReducer(initialRuntimeState, {
    type: "start",
    id: "stream-run",
    conversationId: "conv-1",
    message: "stream",
    startedAt: "2026-06-23T09:00:00.000Z"
  });
  state = runtimeReducer(state, { type: "hydrate_tasks", tasks: [] });

  assert.equal(state.runs.find((run) => run.id === "stream-run")?.origin, "stream");
  assert.equal(state.runs.find((run) => run.id === "stream-run")?.status, "running");
});

test("ensure run creates a scoped target for global task events", () => {
  let state = runtimeReducer(initialRuntimeState, {
    type: "ensure_run",
    conversationId: "conv-1",
    message: "restored task",
    startedAt: "2026-06-23T09:00:00.000Z"
  });
  const runId = state.runs[0].id;

  state = runtimeReducer(state, {
    type: "assistant_delta",
    runId,
    delta: "background answer",
    accumulated: "background answer"
  });

  assert.equal(state.runs.length, 1);
  assert.equal(state.runs[0].conversationId, "conv-1");
  assert.equal(state.runs[0].assistantText, "background answer");
});

test("clear draft removes only the targeted run", () => {
  let state = startRun("run-1", "first", initialRuntimeState, "conv-1");
  state = startRun("run-2", "second", state, "conv-2");

  state = runtimeReducer(state, { type: "clear_draft", runId: "run-1" });

  assert.equal(state.runs.some((run) => run.id === "run-1"), false);
  assert.equal(state.runs.some((run) => run.id === "run-2"), true);
  assert.equal(state.activeRun?.id, "run-2");
});

test("tool activity is auxiliary detail and not duplicated as assistant progress", () => {
  let state = startRun("run-1", "inspect processes");
  state = applyEvents(state, [
    {
      type: "tool_call",
      message: "调用工具: execute",
      data: {
        runtimeEventType: "tool_call_started",
        toolCallId: "call-1",
        toolName: "execute",
        argumentsObj: { command: "ps aux" }
      }
    },
    {
      type: "tool_result",
      message: "{\"stdout\":\"python3 99.9\"}",
      data: {
        runtimeEventType: "tool_call_completed",
        toolCallId: "call-1",
        toolName: "execute",
        result: "{\"stdout\":\"python3 99.9\"}"
      }
    }
  ]);

  assert.equal(assistantProgressUpdates(state.activeRun!).length, 0);
  const cells = runAuxiliaryCells(state.activeRun!);
  assert.equal(cells.length, 1);
  assert.equal(cells[0].label, "已运行命令");
  assert.match(cells[0].detail || "", /python3 99\.9/);
});

test("system lifecycle progress updates are filtered from visible assistant progress", () => {
  let state = startRun("run-1", "inspect processes");
  state = applyEvents(state, [
    {
      type: "assistant_progress_update",
      message: "分析用户输入并准备运行上下文。",
      data: { runtimeEventType: "assistant_progress_update", assistantMessageId: "assistant-1", turnId: "turn-1" }
    },
    {
      type: "assistant_progress_update",
      message: "模型请求执行工具 execute。",
      data: { runtimeEventType: "assistant_progress_update", assistantMessageId: "assistant-1", turnId: "turn-1" }
    },
    {
      type: "assistant_progress_update",
      message: "工具结果已写回上下文，准备继续采样。",
      data: { runtimeEventType: "assistant_progress_update", assistantMessageId: "assistant-1", turnId: "turn-1" }
    }
  ]);

  assert.equal(state.activeRun?.progressUpdates.length, 3);
  assert.deepEqual(assistantProgressUpdates(state.activeRun!), []);
});

test("runtime status updates are trace-only and not assistant progress", () => {
  let state = startRun("run-1", "inspect processes");
  state = applyEvents(state, [
    {
      type: "runtime_status_update",
      message: "工具结果已写回上下文，准备继续采样。",
      data: {
        runtimeEventType: "runtime_status_update",
        runtimeTrace: {
          event: "runtime_status_update",
          message: "工具结果已写回上下文，准备继续采样。",
          turnId: "turn-1"
        }
      }
    }
  ]);

  assert.equal(state.activeRun?.events.length, 1);
  assert.equal(state.activeRun?.events[0].type, "runtime_status_update");
  assert.equal(state.activeRun?.progressUpdates.length, 0);
  assert.deepEqual(runActivityItems(state.activeRun!), []);
});

test("duplicate assistant progress updates are deduped per turn", () => {
  let state = startRun("run-1", "inspect processes");
  state = applyEvents(state, [
    {
      type: "assistant_progress_update",
      message: "正在检查本机进程。",
      data: { runtimeEventType: "assistant_progress_update", assistantMessageId: "assistant-1", turnId: "turn-1" }
    },
    {
      type: "assistant_progress_update",
      message: "正在检查本机进程。",
      data: { runtimeEventType: "assistant_progress_update", assistantMessageId: "assistant-1", turnId: "turn-1" }
    },
    {
      type: "assistant_progress_update",
      message: "正在检查本机进程。",
      data: { runtimeEventType: "assistant_progress_update", assistantMessageId: "assistant-2", turnId: "turn-2" }
    }
  ]);

  assert.deepEqual(
    assistantProgressUpdates(state.activeRun!).map((item) => `${item.turnId}:${item.message}`),
    ["turn-1:正在检查本机进程。", "turn-2:正在检查本机进程。"]
  );
});

test("todo plan remains a separate progress model instead of auxiliary detail", () => {
  let state = startRun("run-1", "inspect processes");
  state = applyEvents(state, [
    {
      type: "planning",
      data: {
        runtimeEventType: "plan_updated",
        items: [
          { id: "1", content: "读取进程列表", status: "completed" },
          { id: "2", content: "判断 CPU 最高进程", status: "in_progress" },
          { id: "3", content: "总结结果", status: "pending" }
        ]
      }
    }
  ]);

  const progress = todoProgress(state.activeRun!.plan);
  assert.equal(progress?.total, 3);
  assert.equal(progress?.completed, 1);
  assert.equal(progress?.current?.content, "判断 CPU 最高进程");
  assert.equal(runAuxiliaryCells(state.activeRun!).some((cell) => cell.kind === "plan"), false);
});

test("markdown checklist planning messages hydrate todo plan items", () => {
  let state = startRun("run-1", "inspect processes");
  state = applyEvents(state, [
    {
      type: "planning",
      message: [
        "- [completed] 读取进程列表",
        "- [in_progress] 判断 CPU 最高进程",
        "- [pending] 总结结果"
      ].join("\n"),
      data: {
        runtimeEventType: "plan_updated"
      }
    }
  ]);

  const progress = todoProgress(state.activeRun!.plan);
  assert.equal(progress?.total, 3);
  assert.equal(progress?.completed, 1);
  assert.equal(progress?.current?.content, "判断 CPU 最高进程");
  assert.deepEqual(
    state.activeRun!.plan.map((item) => item.status),
    ["completed", "in_progress", "pending"]
  );
});

test("todo_updated events hydrate persisted todo plan items", () => {
  let state = startRun("run-1", "inspect processes");
  state = applyEvents(state, [
    {
      type: "todo_updated",
      data: {
        conversationId: "conv-1",
        todos: [
          { itemId: "todo-1", content: "读取进程列表", status: "completed", position: 0 },
          { itemId: "todo-2", content: "判断 CPU 最高进程", status: "in_progress", position: 1 }
        ]
      }
    }
  ]);

  assert.equal(todoProgress(state.activeRun!.plan)?.total, 2);
  assert.equal(state.activeRun!.plan[0].id, "todo-1");
  assert.equal(state.activeRun!.plan[1].status, "in_progress");
});

test("todo_updated does not pollute another conversation run body", () => {
  let state = startRun("run-a", "first", initialRuntimeState, "conv-a");
  state = runtimeReducer(state, {
    type: "start",
    id: "run-b",
    conversationId: "conv-b",
    message: "second",
    startedAt: "2026-06-23T09:00:00.000Z"
  });

  state = applyGlobalTaskEvent(state, "conv-a", {
    type: "todo_updated",
    data: {
      conversationId: "conv-a",
      todos: [{ itemId: "todo-a", content: "A todo", status: "in_progress", position: 0 }]
    }
  });

  assert.equal(state.runs.find((run) => run.conversationId === "conv-a")?.plan[0]?.content, "A todo");
  assert.equal(state.runs.find((run) => run.conversationId === "conv-b")?.plan.length, 0);
  assert.equal(state.runs.find((run) => run.conversationId === "conv-b")?.assistantText, "");
});

test("completed task todo snapshot restores without active task", () => {
  let state = initialRuntimeState;
  state = runtimeReducer(state, {
    type: "plan",
    conversationId: "conv-done",
    status: "completed",
    items: [{ id: "todo-1", content: "总结结果", status: "completed" }]
  });

  const run = state.runs.find((item) => item.conversationId === "conv-done");
  assert.equal(run?.status, "completed");
  assert.equal(todoProgress(run!.plan)?.completed, 1);
});

test("completed task can retain restored todo plan", () => {
  let state = runtimeReducer(initialRuntimeState, {
    type: "ensure_run",
    conversationId: "conv-done",
    message: "run task",
    startedAt: "2026-06-23T09:00:00.000Z",
    origin: "task"
  });
  const runId = state.runs.find((item) => item.conversationId === "conv-done")?.id;
  state = runtimeReducer(state, {
    type: "finish",
    runId,
    status: "completed"
  });
  state = runtimeReducer(state, {
    type: "plan",
    conversationId: "conv-done",
    status: "completed",
    items: [{ id: "todo-1", content: "保留 Todo", status: "completed" }]
  });

  const run = state.runs.find((item) => item.conversationId === "conv-done");
  assert.equal(run?.status, "completed");
  assert.equal(run?.plan[0]?.content, "保留 Todo");
});

test("conversation without todos does not create an empty todo dock run", () => {
  const state = runtimeReducer(initialRuntimeState, {
    type: "plan",
    conversationId: "conv-empty",
    status: "completed",
    items: []
  });

  assert.equal(state.runs.find((item) => item.conversationId === "conv-empty"), undefined);
});

test("update_plan and todowrite tools are not rendered as tool activity parts", () => {
  let state = startRun("run-1", "inspect processes");
  state = applyEvents(state, [
    {
      type: "planning",
      data: {
        runtimeEventType: "plan_updated",
        items: [
          { id: "1", content: "读取进程列表", status: "completed" },
          { id: "2", content: "总结结果", status: "in_progress" }
        ]
      }
    },
    {
      type: "tool_call",
      message: "调用工具: update_plan",
      data: {
        runtimeEventType: "tool_call_started",
        toolCallId: "plan-call-1",
        toolName: "update_plan",
        argumentsObj: { items: [{ content: "读取进程列表", status: "completed" }] }
      }
    },
    {
      type: "tool_result",
      message: "{\"ok\":true}",
      data: {
        runtimeEventType: "tool_call_completed",
        toolCallId: "plan-call-1",
        toolName: "update_plan",
        result: "{\"ok\":true}"
      }
    },
    {
      type: "tool_call",
      message: "调用工具: todowrite",
      data: {
        runtimeEventType: "tool_call_started",
        toolCallId: "plan-call-2",
        toolName: "todowrite",
        argumentsObj: { todos: [{ content: "总结结果", status: "in_progress" }] }
      }
    }
  ]);

  assert.equal(todoProgress(state.activeRun!.plan)?.total, 2);
  assert.deepEqual(runAuxiliaryCells(state.activeRun!), []);
  assert.deepEqual(runActivityItems(state.activeRun!), []);
});

test("run activity interleaves assistant progress and tool cells by event time", () => {
  let state = startRun("run-1", "inspect processes");
  state = applyEvents(state, [
    {
      type: "assistant_progress_update",
      message: "正在检查本机进程。",
      data: { runtimeEventType: "assistant_progress_update", assistantMessageId: "assistant-1", turnId: "turn-1" }
    },
    {
      type: "tool_call",
      message: "调用工具: execute",
      data: {
        runtimeEventType: "tool_call_started",
        toolCallId: "call-1",
        toolName: "execute",
        argumentsObj: { command: "ps aux" }
      }
    }
  ]);

  const items = runActivityItems(state.activeRun!);
  assert.deepEqual(items.map((item) => item.kind), ["progress", "tool"]);
  assert.equal(items[0].kind === "progress" ? items[0].update.message : "", "正在检查本机进程。");
  assert.equal(items[1].kind === "tool" ? items[1].cell.label : "", "已运行命令");
});

test("consecutive tool calls render without requiring assistant progress text", () => {
  let state = startRun("run-1", "inspect processes");
  state = applyEvents(state, [
    {
      type: "tool_call",
      message: "调用工具: execute",
      data: {
        runtimeEventType: "tool_call_started",
        toolCallId: "call-1",
        toolName: "execute",
        argumentsObj: { command: "ps aux" }
      }
    },
    {
      type: "tool_result",
      message: "{\"stdout\":\"python3 80\"}",
      data: {
        runtimeEventType: "tool_call_completed",
        toolCallId: "call-1",
        toolName: "execute",
        result: "{\"stdout\":\"python3 80\"}"
      }
    },
    {
      type: "tool_call",
      message: "调用工具: search_knowledge_base",
      data: {
        runtimeEventType: "tool_call_started",
        toolCallId: "call-2",
        toolName: "search_knowledge_base",
        argumentsObj: { query: "process cpu" }
      }
    },
    {
      type: "tool_result",
      message: "{\"snippets\":[]}",
      data: {
        runtimeEventType: "tool_call_completed",
        toolCallId: "call-2",
        toolName: "search_knowledge_base",
        result: "{\"snippets\":[]}"
      }
    }
  ]);

  assert.deepEqual(assistantProgressUpdates(state.activeRun!), []);
  const items = runActivityItems(state.activeRun!);
  assert.deepEqual(items.map((item) => item.kind), ["tool", "tool"]);
  assert.deepEqual(
    items.map((item) => (item.kind === "tool" ? item.cell.label : "")),
    ["已运行命令", "已查询知识库 1 次"]
  );
});

test("skill tool activity shows the concrete skill name from arguments", () => {
  let state = startRun("run-1", "load skill");
  state = applyEvents(state, [
    {
      type: "tool_call",
      message: "调用工具: skill",
      data: {
        runtimeEventType: "tool_call_started",
        toolCallId: "skill-call-1",
        toolName: "skill",
        argumentsObj: { name: "api-security-testing" }
      }
    },
    {
      type: "tool_result",
      message: "loaded",
      data: {
        runtimeEventType: "tool_call_completed",
        toolCallId: "skill-call-1",
        toolName: "skill",
        result: "loaded"
      }
    }
  ]);

  const cells = runAuxiliaryCells(state.activeRun!);
  assert.equal(cells[0].kind, "skill");
  assert.equal(cells[0].label, "已调用 Skill api-security-testing");
});

test("mcp activity shows builtin or external target name instead of generic wrapper", () => {
  let state = startRun("run-1", "call mcp");
  state = applyEvents(state, [
    {
      type: "tool_call",
      message: "调用工具: mcp_call",
      data: {
        runtimeEventType: "tool_call_started",
        toolCallId: "mcp-call-1",
        toolName: "mcp_call",
        argumentsObj: { tool: "builtin::batch_task_get", arguments: { queue_id: "demo" } }
      }
    },
    {
      type: "tool_result",
      message: "队列不存在",
      data: {
        runtimeEventType: "tool_call_completed",
        toolCallId: "mcp-call-1",
        toolName: "mcp_call",
        result: "队列不存在"
      }
    },
    {
      type: "tool_call",
      message: "调用工具: demo::lookup",
      data: {
        runtimeEventType: "tool_call_started",
        toolCallId: "mcp-call-2",
        toolName: "demo::lookup",
        argumentsObj: { query: "x" }
      }
    }
  ]);

  const cells = runAuxiliaryCells(state.activeRun!);
  assert.deepEqual(
    cells.map((cell) => cell.label),
    ["已调用 MCP builtin::batch_task_get", "已调用 MCP demo::lookup"]
  );
});

test("short builtin mcp tool names are displayed with builtin namespace", () => {
  let state = startRun("run-1", "call builtin");
  state = applyEvents(state, [
    {
      type: "tool_call",
      message: "调用工具: batch_task_get",
      data: {
        runtimeEventType: "tool_call_started",
        toolCallId: "builtin-call-1",
        toolName: "batch_task_get",
        argumentsObj: { queue_id: "demo" }
      }
    }
  ]);

  const cells = runAuxiliaryCells(state.activeRun!);
  assert.equal(cells[0].kind, "mcp");
  assert.equal(cells[0].label, "已调用 MCP builtin::batch_task_get");
});

test("collapsed run summary count includes todo steps without duplicating plan tools", () => {
  let state = startRun("run-1", "inspect processes");
  state = applyEvents(state, [
    {
      type: "planning",
      data: {
        runtimeEventType: "plan_updated",
        items: [
          { id: "1", content: "读取进程列表", status: "completed" },
          { id: "2", content: "总结结果", status: "in_progress" }
        ]
      }
    },
    {
      type: "tool_call",
      message: "调用工具: update_plan",
      data: {
        runtimeEventType: "tool_call_started",
        toolCallId: "plan-call-1",
        toolName: "update_plan",
        argumentsObj: { items: [{ content: "总结结果", status: "in_progress" }] }
      }
    },
    {
      type: "assistant_progress_update",
      message: "我已经拿到进程列表，接下来判断 CPU 使用最高的进程。",
      data: { runtimeEventType: "assistant_progress_update", assistantMessageId: "assistant-1", turnId: "turn-1" }
    },
    {
      type: "tool_call",
      message: "调用工具: execute",
      data: {
        runtimeEventType: "tool_call_started",
        toolCallId: "call-1",
        toolName: "execute",
        argumentsObj: { command: "ps aux" }
      }
    }
  ]);

  assert.deepEqual(runAuxiliaryCells(state.activeRun!), [
    {
      id: "run-1-tool-call-1",
      kind: "command",
      label: "已运行命令",
      detail: '{\n  "command": "ps aux"\n}',
      status: "running",
      time: state.activeRun!.tools["call-1"].startedAt
    }
  ]);
  assert.equal(runActivityItems(state.activeRun!).length, 2);
  assert.equal(runActivityCount(state.activeRun!), 4);
});

test("runtime tool call delta appends to the active command cell while streaming", () => {
  let state = startRun("run-1", "run streaming command");

  state = applyEvents(state, [
    {
      type: "tool_call",
      message: "调用工具: execute",
      data: {
        runtimeEventType: "tool_call_started",
        toolCallId: "call-1",
        toolName: "execute",
        argumentsObj: { command: "for i in 1 2; do echo step-$i; sleep 1; done" },
        runtimeTrace: {
          event: "tool_call_started",
          tool: {
            callId: "call-1",
            name: "execute",
            arguments: { command: "for i in 1 2; do echo step-$i; sleep 1; done" }
          }
        }
      }
    },
    {
      type: "tool_result_delta",
      message: "step-1\n",
      data: {
        runtimeEventType: "tool_call_delta",
        toolCallId: "call-1",
        delta: "step-1\n",
        runtimeTrace: {
          event: "tool_call_delta",
          tool: { callId: "call-1" },
          delta: "step-1\n"
        }
      }
    },
    {
      type: "tool_result_delta",
      message: "step-2\n",
      data: {
        runtimeEventType: "tool_call_delta",
        toolCallId: "call-1",
        delta: "step-2\n",
        runtimeTrace: {
          event: "tool_call_delta",
          tool: { callId: "call-1" },
          delta: "step-2\n"
        }
      }
    }
  ]);

  const cell = runAuxiliaryCells(state.activeRun!)[0];
  assert.equal(cell.status, "running");
  assert.equal(cell.detail, "step-1\nstep-2\n");
});

test("completed execute result renders stdout instead of raw JSON", () => {
  let state = startRun("run-1", "run streaming command");

  state = applyEvents(state, [
    {
      type: "tool_call",
      message: "调用工具: execute",
      data: {
        runtimeEventType: "tool_call_started",
        toolCallId: "call-1",
        toolName: "execute",
        argumentsObj: { command: "for i in 1 2; do echo step-$i; sleep 1; done" }
      }
    },
    {
      type: "tool_result_delta",
      message: "step-1\n",
      data: {
        runtimeEventType: "tool_call_delta",
        toolCallId: "call-1",
        delta: "step-1\n",
        runtimeTrace: {
          event: "tool_call_delta",
          tool: { callId: "call-1" },
          delta: "step-1\n"
        }
      }
    },
    {
      type: "tool_result",
      message: "{\"command\":\"for i in 1 2; do echo step-$i; sleep 1; done\",\"stdout\":\"step-1\\nstep-2\\n\",\"stderr\":\"\",\"success\":true}",
      data: {
        runtimeEventType: "tool_call_completed",
        toolCallId: "call-1",
        toolName: "execute",
        result: "{\"command\":\"for i in 1 2; do echo step-$i; sleep 1; done\",\"stdout\":\"step-1\\nstep-2\\n\",\"stderr\":\"\",\"success\":true}"
      }
    }
  ]);

  const cell = runAuxiliaryCells(state.activeRun!)[0];
  assert.equal(cell.status, "completed");
  assert.equal(cell.detail, "step-1\nstep-2\n");
  assert.doesNotMatch(cell.detail || "", /"stdout"/);
});

test("active run is suppressed when final assistant message already owns restored activity", () => {
  let state = startRun("run-1", "查询快代理官方文档", initialRuntimeState, "conv-1");
  state = runtimeReducer(state, {
    type: "assistant_delta",
    runId: "run-1",
    delta: "我这里无法直接联网实时打开快代理官网",
    accumulated: "我这里无法直接联网实时打开快代理官网"
  });
  state = runtimeReducer(state, {
    type: "progress_update",
    runId: "run-1",
    update: {
      id: "progress-1",
      message: "正在整理快代理接口参数。",
      time: "2026-06-23T09:00:01.000Z",
      assistantMessageId: "assistant-1",
      turnId: "turn-1"
    }
  });

  const messages = [
    {
      id: "user-1",
      conversationId: "conv-1",
      role: "user",
      content: "查询快代理官方文档"
    },
    {
      id: "assistant-1",
      conversationId: "conv-1",
      role: "assistant",
      content: "我这里无法直接联网实时打开快代理官网"
    }
  ];
  const runsByMessageId = {
    "assistant-1": {
      id: "restored-assistant-1",
      conversationId: "conv-1",
      assistantMessageId: "assistant-1",
      assistantText: "",
      reasoningText: "",
      progressUpdates: [],
      status: "completed" as const,
      plan: [],
      tools: {},
      approvals: [],
      events: [],
      startedAt: "2026-06-23T09:00:00.000Z",
      completedAt: "2026-06-23T09:01:00.000Z"
    }
  };

  assert.equal(lastUserIndex(messages), 0);
  assert.equal(activeRunAssistantMessageId(messages, state.activeRun, runsByMessageId), "assistant-1");
});

test("completed active run remains attached to the final assistant message before details hydrate", () => {
  let state = startRun("run-1", "run command", initialRuntimeState, "conv-1");
  state = runtimeReducer(state, {
    type: "tool",
    runId: "run-1",
    tool: {
      id: "call-1",
      name: "execute",
      status: "completed",
      output: "STREAM_DONE\n"
    }
  });
  state = runtimeReducer(state, {
    type: "assistant_delta",
    runId: "run-1",
    delta: "",
    accumulated: "final answer"
  });
  state = runtimeReducer(state, {
    type: "finish",
    runId: "run-1",
    status: "completed"
  });

  const messages = [
    {
      id: "user-1",
      conversationId: "conv-1",
      role: "user",
      content: "run command"
    },
    {
      id: "assistant-1",
      conversationId: "conv-1",
      role: "assistant",
      content: "final answer"
    }
  ];

  assert.equal(activeRunAssistantMessageId(messages, state.activeRun, {}), "assistant-1");
});

test("active run is still inserted before any assistant final message exists", () => {
  const state = startRun("run-1", "查询快代理官方文档", initialRuntimeState, "conv-1");
  const messages = [
    {
      id: "user-1",
      conversationId: "conv-1",
      role: "user",
      content: "查询快代理官方文档"
    }
  ];

  assert.equal(lastUserIndex(messages), 0);
  assert.equal(activeRunAssistantMessageId(messages, state.activeRun, {}), "");
});
