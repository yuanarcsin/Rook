import { describe, expect, it } from "vitest";

import {
  LOG_POLL_MS,
  TAIL_OPTIONS,
  filterLogLines,
  isStaleResponse,
  nextRequestSeq,
  shouldAutoScroll,
  shouldClearLoading,
  shouldPoll,
  shouldResetFollow,
} from "$lib/utils/log-poll";

describe("请求序号", () => {
  it("每次读取递增序号", () => {
    expect(nextRequestSeq(0)).toBe(1);
    expect(nextRequestSeq(41)).toBe(42);
  });

  it("旧序号的响应被视为过期", () => {
    expect(isStaleResponse(1, 2)).toBe(true);
    expect(isStaleResponse(2, 2)).toBe(false);
  });

  it("只有最新一次非静默请求结束 loading", () => {
    expect(shouldClearLoading(2, 2, false)).toBe(true);
    expect(shouldClearLoading(1, 2, false)).toBe(false);
    expect(shouldClearLoading(2, 2, true)).toBe(false);
  });
});

describe("轮询与滚动判定", () => {
  it("自动刷新关闭或没有选中任务时不轮询", () => {
    expect(shouldPoll(true, "ykt")).toBe(true);
    expect(shouldPoll(false, "ykt")).toBe(false);
    expect(shouldPoll(true, null)).toBe(false);
  });

  it("跟随开关与是否有内容共同决定自动滚动", () => {
    expect(shouldAutoScroll(true, true)).toBe(true);
    expect(shouldAutoScroll(false, true)).toBe(false);
    expect(shouldAutoScroll(true, false)).toBe(false);
  });

  it("文件标识变化时重置跟随，首屏不算切换", () => {
    expect(shouldResetFollow(null, "a.log")).toBe(false);
    expect(shouldResetFollow("a.log", "a.log")).toBe(false);
    expect(shouldResetFollow("a.log", "b.log")).toBe(true);
  });

  it("轮询间隔与行数选项保持原值", () => {
    expect(LOG_POLL_MS).toBe(2000);
    expect(TAIL_OPTIONS).toEqual([100, 200, 500, 1000]);
  });
});

describe("filterLogLines", () => {
  it("空查询返回原数组", () => {
    const lines = ["a", "b"];
    expect(filterLogLines(lines, "   ")).toBe(lines);
  });

  it("大小写不敏感并去掉查询两端空白", () => {
    expect(filterLogLines(["Error: x", "ok"], " error ")).toEqual(["Error: x"]);
  });

  it("没有命中时返回空数组", () => {
    expect(filterLogLines(["a", "b"], "zzz")).toEqual([]);
  });
});
