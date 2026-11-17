import { describe, expect, it } from "vitest";

import {
  filterEventLines,
  isStateTransition,
  parseEventLine,
} from "$lib/utils/event-filter";

/** 固定本地时间，避免测试依赖运行时时区。 */
const TS = new Date(2026, 0, 2, 3, 4, 5).toISOString();
const TIME = "03:04:05";

function line(
  event: string,
  task: string | null = null,
  extra: Record<string, unknown> = {},
  ts: unknown = TS,
): string {
  return JSON.stringify({
    ts,
    event,
    message: `${event} happened`,
    fields: { ...(task === null ? {} : { task }), ...extra },
  });
}

describe("parseEventLine", () => {
  it("解析基本字段与任务", () => {
    const record = parseEventLine(
      line("task.decision", "alpha", { reason: "exit_code", code: 1 }),
    );
    expect(record).not.toBeNull();
    expect(record?.time).toBe(TIME);
    expect(record?.event).toBe("task.decision");
    expect(record?.task).toBe("alpha");
    expect(record?.message).toBe("task.decision happened");
    expect(record?.extra).toBe("task=alpha reason=exit_code code=1");
  });

  it("非法 JSON 与空行返回 null", () => {
    expect(parseEventLine("not json")).toBeNull();
    expect(parseEventLine("")).toBeNull();
    expect(parseEventLine("   ")).toBeNull();
  });

  it("非对象 JSON 返回 null", () => {
    expect(parseEventLine("42")).toBeNull();
    expect(parseEventLine("null")).toBeNull();
  });

  it("缺少 fields.task 时为 null，数字 task 转字符串", () => {
    expect(parseEventLine(line("app.start"))?.task).toBeNull();
    expect(parseEventLine(line("app.start", "7"))?.task).toBe("7");
  });

  it("没有 event 时退回 level，时间戳无效时占位", () => {
    const raw = JSON.stringify({ ts: "bad", level: "warn", fields: {} });
    const record = parseEventLine(raw);
    expect(record?.event).toBe("warn");
    expect(record?.time).toBe("--:--:--");
  });
});

describe("isStateTransition", () => {
  it("固定集合内的事件算迁移", () => {
    expect(isStateTransition("task.decision")).toBe(true);
    expect(isStateTransition("task.start")).toBe(true);
    expect(isStateTransition("task.exit")).toBe(true);
    expect(isStateTransition("task.pause")).toBe(true);
    expect(isStateTransition("task.resume")).toBe(true);
    expect(isStateTransition("schedule.window_enter")).toBe(true);
    expect(isStateTransition("schedule.window_exit")).toBe(true);
  });

  it("restart./health. 前缀算迁移", () => {
    expect(isStateTransition("restart.backoff")).toBe(true);
    expect(isStateTransition("health.degraded")).toBe(true);
  });

  it("非迁移事件排除，且前缀必须带点", () => {
    expect(isStateTransition("task.log")).toBe(false);
    expect(isStateTransition("app.start")).toBe(false);
    expect(isStateTransition("schedule.tick")).toBe(false);
    expect(isStateTransition("restarts")).toBe(false);
  });
});

describe("filterEventLines", () => {
  const lines = [
    line("task.decision", "alpha", { reason: "exit_code" }),
    line("task.log", "alpha"),
    line("restart.backoff", "beta"),
    line("app.start"),
    "not json",
    "",
  ];

  it("无过滤条件时保留所有行（含空行与坏行）", () => {
    expect(filterEventLines(lines, {
      taskId: null,
      transitionsOnly: false,
      search: "",
    })).toHaveLength(lines.length);
  });

  it("按任务过滤，隐藏无 task 与解析失败的行", () => {
    const result = filterEventLines(lines, {
      taskId: "alpha",
      transitionsOnly: false,
      search: "",
    });
    expect(result).toEqual([lines[0], lines[1]]);
  });

  it("只看状态迁移", () => {
    const result = filterEventLines(lines, {
      taskId: null,
      transitionsOnly: true,
      search: "",
    });
    expect(result).toEqual([lines[0], lines[2]]);
  });

  it("搜索大小写不敏感且匹配原始行", () => {
    const result = filterEventLines(lines, {
      taskId: null,
      transitionsOnly: false,
      search: "TASK.LOG",
    });
    expect(result).toEqual([lines[1]]);
  });

  it("组合过滤：任务 + 迁移 + 搜索", () => {
    const result = filterEventLines(lines, {
      taskId: "alpha",
      transitionsOnly: true,
      search: "reason",
    });
    expect(result).toEqual([lines[0]]);
    expect(filterEventLines(lines, {
      taskId: "alpha",
      transitionsOnly: true,
      search: "beta",
    })).toEqual([]);
  });

  it("空白搜索等价于不过滤", () => {
    expect(filterEventLines(lines, {
      taskId: null,
      transitionsOnly: false,
      search: "   ",
    })).toHaveLength(lines.length);
  });

  it("空输入返回空数组", () => {
    expect(filterEventLines([], {
      taskId: "alpha",
      transitionsOnly: true,
      search: "x",
    })).toEqual([]);
  });
});
