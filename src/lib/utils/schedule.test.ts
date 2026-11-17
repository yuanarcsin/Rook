import { describe, expect, it } from "vitest";

import type { ScheduleDraft, ScheduleWindowDraft } from "$lib/api";
import { describeSchedule, validateSchedule, weekdayShort } from "$lib/utils/schedule";

function window_(patch: Partial<ScheduleWindowDraft> = {}): ScheduleWindowDraft {
  return { days: ["mon"], start: "07:50", end: "12:00", ...patch };
}

function draft(patch: Partial<ScheduleDraft> = {}): ScheduleDraft {
  return {
    enabled: true,
    windows: [window_()],
    ...patch,
  };
}

describe("describeSchedule", () => {
  it("无时段提示未配置", () => {
    expect(describeSchedule([])).toBe("未配置时段");
  });

  it("单窗口按中文星期拼接", () => {
    expect(describeSchedule([window_()])).toBe("周一 07:50–12:00");
  });

  it("多窗口用分号连接，多星期用顿号连接", () => {
    expect(
      describeSchedule([
        window_({ days: ["mon", "tue"] }),
        window_({ days: ["sat"], start: "09:00", end: "18:00" }),
      ]),
    ).toBe("周一、周二 07:50–12:00；周六 09:00–18:00");
  });

  it("连续三天以上压缩为区间，同一天多个时段合并", () => {
    expect(
      describeSchedule([
        window_({ days: ["mon", "tue", "wed", "thu", "fri"], start: "07:50", end: "12:10" }),
        window_({ days: ["mon", "tue", "wed", "thu", "fri"], start: "13:50", end: "18:10" }),
        window_({ days: ["sat", "sun"], start: "09:00", end: "18:00" }),
      ]),
    ).toBe("周一至周五 07:50–12:10、13:50–18:10；周六、周日 09:00–18:00");
  });

  it("未知星期原样显示，未选星期给出提示", () => {
    expect(describeSchedule([window_({ days: ["holiday"] })])).toBe(
      "holiday 07:50–12:00",
    );
    expect(describeSchedule([window_({ days: [] })])).toBe(
      "未选星期 07:50–12:00",
    );
  });
});

describe("validateSchedule", () => {
  it("关闭时段时任何草稿都通过", () => {
    expect(validateSchedule(draft({ enabled: false, windows: [] }))).toBeNull();
  });

  it("开启但没有时段返回可读错误", () => {
    expect(validateSchedule(draft({ windows: [] }))).toContain("还没有添加任何时段");
  });

  it("时段没有选择星期时指出序号", () => {
    expect(validateSchedule(draft({ windows: [window_({ days: [] })] }))).toBe(
      "第 1 个时段没有选择星期。",
    );
    expect(
      validateSchedule(
        draft({ windows: [window_(), window_({ days: [] })] }),
      ),
    ).toBe("第 2 个时段没有选择星期。");
  });

  it("时间格式非法时指出序号", () => {
    expect(validateSchedule(draft({ windows: [window_({ start: "7:50" })] }))).toBe(
      "第 1 个时段的时间格式应为 HH:MM。",
    );
    expect(validateSchedule(draft({ windows: [window_({ end: "24:00" })] }))).toBe(
      "第 1 个时段的时间格式应为 HH:MM。",
    );
    expect(validateSchedule(draft({ windows: [window_({ start: "0750" })] }))).toBe(
      "第 1 个时段的时间格式应为 HH:MM。",
    );
  });

  it("开始与结束相同视为错误（零长窗口）", () => {
    expect(
      validateSchedule(draft({ windows: [window_({ start: "08:00", end: "08:00" })] })),
    ).toBe("第 1 个时段的开始与结束时间相同。");
  });

  it("跨午夜与合法窗口都通过", () => {
    expect(
      validateSchedule(draft({ windows: [window_({ start: "22:00", end: "02:00" })] })),
    ).toBeNull();
    expect(
      validateSchedule(
        draft({
          windows: [
            window_({ days: ["mon", "tue"], start: "00:00", end: "23:59" }),
            window_({ days: ["sun"], start: "08:30", end: "09:00" }),
          ],
        }),
      ),
    ).toBeNull();
  });
});

describe("weekdayShort", () => {
  it("已知星期返回单字，未知值原样返回", () => {
    expect(weekdayShort("mon")).toBe("一");
    expect(weekdayShort("sun")).toBe("日");
    expect(weekdayShort("holiday")).toBe("holiday");
  });
});
