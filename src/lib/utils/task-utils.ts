// 前端共用的任务状态派生与格式化工具（Svelte 无关，便于复用与单测）。
//
// 状态展示规则与 docs/03 状态机、托盘菜单保持一致：界面不允许后端会拒绝的动作。

import type { AutostartStatus, PausedReason, TaskSnapshot } from "$lib/api";

export type TaskAction = "start" | "stop" | "restart";

/** 操作按钮文案（空闲 / 进行中）。 */
export const taskActionLabel: Record<TaskAction, string> = {
  start: "启动",
  stop: "停止",
  restart: "重启",
};

export const taskActionBusyLabel: Record<TaskAction, string> = {
  start: "启动中…",
  stop: "停止中…",
  restart: "重启中…",
};

/** 提权未实现的统一说明（docs/13 L1/L10）。 */
export const ELEVATED_NOTICE =
  "当前版本未实现按脚本提权：勾选或配置 elevated 不会改变实际权限，脚本仍以普通权限运行。";

/** 从任意错误值提取可读消息，永不返回空串。 */
export function errorMessage(error: unknown): string {
  if (typeof error === "object" && error !== null && "message" in error) {
    const message = (error as { message?: unknown }).message;
    if (typeof message === "string" && message.length > 0) {
      return message;
    }
  }
  if (typeof error === "string" && error.length > 0) {
    return error;
  }
  return String(error);
}

/**
 * 是否允许执行某个动作。
 *
 * 优先使用后端快照下发的 `allowedActions`（权威规则）；字段缺失时退回本地推导，
 * 只为兼容旧快照与测试夹具。界面不允许后端会拒绝的动作。
 */
export function canRun(task: TaskSnapshot, action: TaskAction): boolean {
  const allowed = task.allowedActions?.[action];
  if (typeof allowed === "boolean") {
    return allowed;
  }
  const status = task.runtime?.status;
  if (!task.valid || !status) {
    return false;
  }
  // enabled=false 不可手动启动/重启（docs/02 §4）；Stop 仍允许收敛运行中实例。
  if (action !== "stop" && !task.enabled) {
    return false;
  }
  switch (action) {
    case "start":
      return status === "stopped";
    case "stop":
      return status === "running" || status === "stopping";
    case "restart":
      return status === "stopped" || status === "running";
  }
}

/**
 * 展示状态：优先使用后端算出的 `displayStatus`（与托盘同一套规则）；
 * 字段缺失时退回本地推导，兼容旧快照。
 */
export function displayStatus(task: TaskSnapshot): string {
  if (task.displayStatus) {
    return task.displayStatus;
  }
  const status = task.runtime?.status;
  const supervision = task.supervisor.lastStatus;
  if (status === "running" && supervision === "starting") {
    return "starting";
  }
  if (status && status !== "stopped") {
    return status;
  }
  if (task.supervisor.needsHuman.active) {
    return "needs_human";
  }
  if (task.supervisor.pausedReason) {
    return "paused";
  }
  if (supervision === "backoff") {
    return "backoff";
  }
  return status ?? "unknown";
}

export const displayStatusLabel: Record<string, string> = {
  invalid: "无效",
  starting: "启动中",
  running: "运行中",
  stopping: "停止中",
  stopped: "已停止",
  paused: "已暂停",
  needs_human: "需要人工处理",
  backoff: "退避重试",
  disabled: "已禁用",
  unknown: "—",
};

/** 状态色调：ok（绿）/ info（蓝）/ warn（黄）/ bad（红）/ muted（灰）。 */
export type StatusTone = "ok" | "info" | "warn" | "bad" | "muted";

export function statusTone(status: string): StatusTone {
  switch (status) {
    case "running":
      return "ok";
    case "starting":
      return "info";
    case "stopping":
      return "warn";
    case "backoff":
      return "warn";
    case "needs_human":
      return "warn";
    case "paused":
      return "bad";
    default:
      return "muted";
  }
}

/** 需要人工处理/配置有误的任务（监控顶部汇总用）。 */
export function needsAttention(task: TaskSnapshot): boolean {
  return (
    !task.valid ||
    task.supervisor.needsHuman.active ||
    task.supervisor.pausedReason !== null
  );
}

/**
 * 暂停原因 → 短标签（与后端 `PausedReason::as_str()` 逐字一致）。
 *
 * `satisfies Record<PausedReason, string>` 保证 union 里每个取值都有标签：
 * 后端新增取值而这里漏写时，`pnpm check` 直接报错。
 */
export const PAUSED_REASON_LABEL = {
  crash_loop: "连续崩溃",
  crash_no_restart: "退出后不重启",
  restart_budget: "重启过于频繁",
  needs_human: "需要人工处理",
  user_pause: "手动暂停",
  elevation_unavailable: "提权不可用",
} satisfies Record<PausedReason, string>;

/** 旧版 state.json 的历史取值（后端已不再产出），只为读老文件保留。 */
export const LEGACY_PAUSED_REASON_LABEL: Record<string, string> = {
  user: "手动暂停",
  port_conflict: "端口冲突",
  elevation: "提权不可用",
};

/**
 * 暂停原因 → 短标签（未知值原样显示，便于排障）。
 *
 * 先查当前取值，再查历史取值，最后原样返回。
 */
export function pausedReasonLabel(reason: string): string {
  return (
    PAUSED_REASON_LABEL[reason as PausedReason] ??
    LEGACY_PAUSED_REASON_LABEL[reason] ??
    reason
  );
}

/** 汇总：运行中 / 需处理 / 已停止 / 总任务数。 */
export function summarizeTasks(tasks: TaskSnapshot[]): {
  total: number;
  running: number;
  attention: number;
  stopped: number;
} {
  let running = 0;
  let attention = 0;
  let stopped = 0;
  for (const task of tasks) {
    const status = displayStatus(task);
    if (needsAttention(task)) {
      attention += 1;
    }
    if (status === "running" || status === "starting" || status === "stopping") {
      running += 1;
    } else if (!needsAttention(task)) {
      stopped += 1;
    }
  }
  return { total: tasks.length, running, attention, stopped };
}

/** 字节数 → 人类可读文本。 */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} KB`;
  }
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** 四态文案：已注册 / 未注册 / 路径不匹配 / 读取失败。 */
export function autostartLabel(status: AutostartStatus): string {
  if (status.error) {
    return "读取失败";
  }
  if (!status.registered) {
    return "未注册";
  }
  return status.matches ? "已注册" : "路径不匹配";
}

export const autostartModeLabel: Record<string, string> = {
  run_key: "Run 键",
  scheduled_task: "计划任务（v1.1）",
  disabled: "已禁用",
};

/**
 * 复制文本到剪贴板。优先 Clipboard API；WebView 拒绝时退回隐藏 textarea，
 * 仍失败返回 false，由界面提示用户手动复制（不静默失败）。
 */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (typeof navigator !== "undefined" && navigator.clipboard) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // 继续尝试降级方案。
  }
  try {
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    const ok = document.execCommand("copy");
    area.remove();
    return ok;
  } catch {
    return false;
  }
}

export type TextSegment = { text: string; hit: boolean };

/** 把一行文本切成"命中/未命中"片段；用于日志搜索高亮，避免 {@html}。 */
export function highlightText(line: string, query: string): TextSegment[] {
  const needle = query.trim();
  if (needle.length === 0) {
    return [{ text: line, hit: false }];
  }
  const lowerLine = line.toLowerCase();
  const lowerNeedle = needle.toLowerCase();
  const segments: TextSegment[] = [];
  let index = 0;
  let hits = 0;
  while (hits < 200) {
    const found = lowerLine.indexOf(lowerNeedle, index);
    if (found < 0) {
      break;
    }
    if (found > index) {
      segments.push({ text: line.slice(index, found), hit: false });
    }
    segments.push({ text: line.slice(found, found + needle.length), hit: true });
    index = found + needle.length;
    hits += 1;
  }
  if (index < line.length) {
    segments.push({ text: line.slice(index), hit: false });
  }
  return segments.length > 0 ? segments : [{ text: line, hit: false }];
}

/** 搜索命中总数（大小写不敏感）。 */
export function countMatches(lines: string[], query: string): number {
  const needle = query.trim().toLowerCase();
  if (needle.length === 0) {
    return 0;
  }
  let total = 0;
  for (const line of lines) {
    const lower = line.toLowerCase();
    let index = 0;
    while (total < 1000) {
      const found = lower.indexOf(needle, index);
      if (found < 0) {
        break;
      }
      total += 1;
      index = found + needle.length;
    }
  }
  return total;
}
