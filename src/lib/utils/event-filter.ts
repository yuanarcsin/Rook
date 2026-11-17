/**
 * 应用事件日志（JSONL）的纯解析/过滤逻辑。
 *
 * 解析规则与 `EventLogViewer` 原实现保持一致：取 `fields.task` 等字段，
 * 时间戳用本地时区的 HH:MM:SS；解析失败返回 null，由 UI 回退原文。
 */

/** 一行事件的紧凑视图（只保留展示与过滤需要的字段）。 */
export interface EventRecord {
  /** 本地时间 HH:MM:SS；无有效 ts 时为 `--:--:--`。 */
  time: string;
  /** `record.event`，退回 `record.level`，都没有时为 `—`。 */
  event: string;
  /** `fields.task`；缺失时为 null。 */
  task: string | null;
  message: string;
  /** 拼好的 `task=… reason=… code=…`。 */
  extra: string;
}

export interface EventFilterOptions {
  /** null 表示全部任务。 */
  taskId: string | null;
  transitionsOnly: boolean;
  search: string;
}

/** 监督状态相关事件（精确匹配）。 */
const STATE_EVENTS = new Set([
  "task.decision",
  "task.start",
  "task.stop",
  "task.exit",
  "task.pause",
  "task.resume",
  "schedule.window_enter",
  "schedule.window_exit",
]);

/** 监督状态相关事件（前缀匹配，如 `restart.backoff`、`health.degraded`）。 */
const STATE_EVENT_PREFIXES = ["restart.", "health."];

function pad(value: number): string {
  return String(value).padStart(2, "0");
}

function formatTime(ts: unknown): string {
  if (typeof ts !== "string") {
    return "--:--:--";
  }
  const date = new Date(ts);
  if (Number.isNaN(date.getTime())) {
    return "--:--:--";
  }
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

function optionalString(value: unknown): string | null {
  if (value === undefined || value === null) {
    return null;
  }
  const text = String(value);
  return text.length > 0 ? text : null;
}

/** 解析一行 JSONL；空行或非法 JSON 返回 null。 */
export function parseEventLine(raw: string): EventRecord | null {
  if (raw.trim().length === 0) {
    return null;
  }
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null) {
    return null;
  }
  const record = value as Record<string, unknown>;
  const fields =
    typeof record.fields === "object" && record.fields !== null
      ? (record.fields as Record<string, unknown>)
      : {};
  const parts: string[] = [];
  for (const key of ["task", "reason", "code"]) {
    const text = optionalString(fields[key]);
    if (text !== null) {
      parts.push(`${key}=${text}`);
    }
  }
  return {
    time: formatTime(record.ts),
    event:
      typeof record.event === "string"
        ? record.event
        : typeof record.level === "string"
          ? record.level
          : "—",
    task: optionalString(fields.task),
    message: typeof record.message === "string" ? record.message : "",
    extra: parts.join(" "),
  };
}

/** 是否是监督状态迁移事件（`task.*` 固定集合 + `restart.*`/`health.*` 前缀）。 */
export function isStateTransition(event: string): boolean {
  return (
    STATE_EVENTS.has(event) ||
    STATE_EVENT_PREFIXES.some((prefix) => event.startsWith(prefix))
  );
}

/** 按任务、状态迁移、搜索（原始行大小写不敏感）组合过滤。 */
export function filterEventLines(
  lines: string[],
  options: EventFilterOptions,
): string[] {
  const needle = options.search.trim().toLowerCase();
  return lines.filter((raw) => {
    if (needle.length > 0 && !raw.toLowerCase().includes(needle)) {
      return false;
    }
    if (options.taskId === null && !options.transitionsOnly) {
      return true;
    }
    const record = parseEventLine(raw);
    // 解析失败 / 空行只在“全部任务 + 不限迁移”下保留。
    if (record === null) {
      return false;
    }
    if (options.taskId !== null && record.task !== options.taskId) {
      return false;
    }
    if (options.transitionsOnly && !isStateTransition(record.event)) {
      return false;
    }
    return true;
  });
}
