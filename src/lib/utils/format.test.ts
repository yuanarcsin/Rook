import { describe, expect, it } from "vitest";

import {
  MISSING,
  formatCountdown,
  formatElapsed,
  formatTimestamp,
  shortId,
} from "$lib/utils/format";

describe("formatTimestamp", () => {
  it("缺失或非法值统一返回占位符", () => {
    expect(formatTimestamp(null)).toBe(MISSING);
    expect(formatTimestamp(undefined)).toBe(MISSING);
    expect(formatTimestamp(Number.NaN)).toBe(MISSING);
    expect(formatTimestamp(Number.POSITIVE_INFINITY)).toBe(MISSING);
  });

  it("正常时按本地时间格式化", () => {
    const ms = Date.UTC(2026, 10, 14, 3, 4, 5);
    expect(formatTimestamp(ms)).toBe(new Date(ms).toLocaleString());
    // epoch 0 是合法时间戳，不能被当成缺失。
    expect(formatTimestamp(0)).not.toBe(MISSING);
  });
});

describe("shortId", () => {
  it("缺失或空串返回占位符", () => {
    expect(shortId(null)).toBe(MISSING);
    expect(shortId(undefined)).toBe(MISSING);
    expect(shortId("")).toBe(MISSING);
  });

  it("不超过 8 位原样返回，超过则截断加省略号", () => {
    expect(shortId("abcd1234")).toBe("abcd1234");
    expect(shortId("abcd12345")).toBe("abcd1234…");
  });
});

describe("formatCountdown", () => {
  it("缺失时间戳或当前时间非法返回占位符", () => {
    expect(formatCountdown(null, 1000)).toBe(MISSING);
    expect(formatCountdown(1000, Number.NaN)).toBe(MISSING);
  });

  it("已到点（含正好到点）显示已到点", () => {
    expect(formatCountdown(1000, 2000)).toBe("已到点");
    expect(formatCountdown(1000, 1000)).toBe("已到点");
  });

  it("不足一小时显示 mm:ss（向上取整）", () => {
    expect(formatCountdown(45_000, 0)).toBe("00:45");
    expect(formatCountdown(90_000, 0)).toBe("01:30");
    expect(formatCountdown(500, 0)).toBe("00:01");
  });

  it("超过一小时显示 h:mm:ss", () => {
    expect(formatCountdown(3_600_000, 0)).toBe("1:00:00");
    expect(formatCountdown(3_909_000, 0)).toBe("1:05:09");
  });
});

describe("formatElapsed", () => {
  it("缺失时间戳返回占位符", () => {
    expect(formatElapsed(null, 1000)).toBe(MISSING);
    expect(formatElapsed(0, Number.NaN)).toBe(MISSING);
  });

  it("秒 / 分钟粒度", () => {
    expect(formatElapsed(0, 12_000)).toBe("12s");
    expect(formatElapsed(0, 59_999)).toBe("59s");
    expect(formatElapsed(0, 60_000)).toBe("1m");
    expect(formatElapsed(0, 90_000)).toBe("1m");
  });

  it("小时 / 天粒度", () => {
    expect(formatElapsed(0, 2 * 3_600_000)).toBe("2h");
    expect(formatElapsed(0, (2 * 3600 + 13 * 60) * 1000)).toBe("2h 13m");
    expect(formatElapsed(0, 24 * 3_600_000)).toBe("1d 0h");
    expect(formatElapsed(0, 28 * 3_600_000)).toBe("1d 4h");
  });

  it("时钟回拨时按 0 秒处理，不产生负值", () => {
    expect(formatElapsed(5000, 1000)).toBe("0s");
  });
});
