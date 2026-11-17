// 运行时段的展示与校验（纯函数，无 Svelte / 无传输层依赖）。
//
// `[tasks.schedule]` 是任务唯一的时钟：`enabled=true` 时由应用唯一接管启停。
// v2（2026-11-15）起 DTO 只有 `enabled + windows`；展示层与编辑器也只处理这两项。

import type { ScheduleDraft, ScheduleWindowDraft } from "$lib/api";

/** 星期标签（显示顺序 mon..sun）。 */
export const WEEKDAY_OPTIONS: { value: string; label: string; short: string }[] = [
  { value: "mon", label: "周一", short: "一" },
  { value: "tue", label: "周二", short: "二" },
  { value: "wed", label: "周三", short: "三" },
  { value: "thu", label: "周四", short: "四" },
  { value: "fri", label: "周五", short: "五" },
  { value: "sat", label: "周六", short: "六" },
  { value: "sun", label: "周日", short: "日" },
];

const weekdayLabel = new Map(WEEKDAY_OPTIONS.map((day) => [day.value, day.label]));

const weekdayIndex = new Map(WEEKDAY_OPTIONS.map((day, index) => [day.value, index]));

/** 星期集合压缩：周一至周五 / 周一、周三 / 未选星期。 */
function describeDays(days: string[]): string {
  if (days.length === 0) {
    return "未选星期";
  }
  const known = WEEKDAY_OPTIONS.map((day) => day.value).filter((value) => days.includes(value));
  const unknown = days.filter((value) => !weekdayIndex.has(value));
  const parts: string[] = [];
  let index = 0;
  while (index < known.length) {
    let end = index;
    while (
      end + 1 < known.length &&
      weekdayIndex.get(known[end + 1]) === weekdayIndex.get(known[end])! + 1
    ) {
      end += 1;
    }
    const count = end - index + 1;
    if (count >= 3) {
      parts.push(weekdayLabel.get(known[index]) + "至" + weekdayLabel.get(known[end]));
    } else {
      parts.push(...known.slice(index, end + 1).map((day) => weekdayLabel.get(day) ?? day));
    }
    index = end + 1;
  }
  parts.push(...unknown);
  return parts.join("、");
}

/**
 * 时段摘要：同一天的多个时段合并，连续三天以上压成"周一至周五"。
 * 例："周一至周五 07:50–12:10、13:50–18:10；周六 09:00–18:00"。
 */
export function describeSchedule(windows: ScheduleWindowDraft[]): string {
  if (windows.length === 0) {
    return "未配置时段";
  }
  const groups = new Map<string, { days: string[]; ranges: string[] }>();
  const order: string[] = [];
  for (const window of windows) {
    const key = window.days.join(",");
    if (!groups.has(key)) {
      groups.set(key, { days: [...window.days], ranges: [] });
      order.push(key);
    }
    groups.get(key)!.ranges.push(window.start + "–" + window.end);
  }
  return order
    .map((key) => {
      const group = groups.get(key)!;
      return describeDays(group.days) + " " + group.ranges.join("、");
    })
    .join("；");
}

const timePattern = /^([01]\d|2[0-3]):[0-5]\d$/;

/**
 * 校验草稿：开启时段时至少一个时段、每个时段有星期且起止时间合法。
 * 返回可读错误；合法返回 null。跨午夜（start > end）是合法的。
 */
export function validateSchedule(draft: ScheduleDraft): string | null {
  if (!draft.enabled) {
    return null;
  }
  if (draft.windows.length === 0) {
    return "已开启运行时段，但还没有添加任何时段；请至少添加一个，或关闭运行时段。";
  }
  for (let index = 0; index < draft.windows.length; index += 1) {
    const window = draft.windows[index];
    if (window.days.length === 0) {
      return `第 ${index + 1} 个时段没有选择星期。`;
    }
    if (!timePattern.test(window.start) || !timePattern.test(window.end)) {
      return `第 ${index + 1} 个时段的时间格式应为 HH:MM。`;
    }
    if (window.start === window.end) {
      return `第 ${index + 1} 个时段的开始与结束时间相同。`;
    }
  }
  return null;
}

/** 星期标签（短）用于任务卡片摘要。 */
export function weekdayShort(day: string): string {
  return WEEKDAY_OPTIONS.find((option) => option.value === day)?.short ?? day;
}
