// TaskForm 的表单模型与纯转换逻辑（Svelte/DOM 无关，便于复用与单测）。
//
// 校验文案里的字段名沿用 docs/02 的 TOML 键名（如 restart.window_ms），
// 与后端 `TaskDraftError` 的展示保持一致；校验失败直接抛 `Error(message)`。

import type { TaskDraft, TaskDraftError, TaskDraftResult } from "$lib/api";
import { errorMessage } from "$lib/utils/task-utils";

export type DraftForm = {
  id: string;
  name: string;
  enabled: boolean;
  command: string;
  /** 每行一个参数。 */
  args: string;
  workingDir: string;
  autoStart: boolean;
  elevated: boolean;
  notify: boolean;
  /** 逗号分隔的端口。 */
  ports: string;
  /** 每行一个 KEY=VALUE。 */
  env: string;
  envProxy: string;
  panelUrl: string;
  health: {
    kind: string;
    intervalMs: string;
    timeoutMs: string;
    startupGraceMs: string;
    failureThreshold: string;
    path: string;
    maxAgeMs: string;
    /** 逗号分隔。 */
    needsHumanStatus: string;
    url: string;
    /** 逗号分隔。 */
    expectStatus: string;
    expectBodyContains: string;
  };
  restart: {
    mode: string;
    initialDelayMs: string;
    maxDelayMs: string;
    multiplier: string;
    jitter: string;
    maxConsecutiveFailures: string;
    /** 逗号分隔。 */
    pauseOnExitCodes: string;
    stableAfterMs: string;
    windowMs: string;
    maxStartsInWindow: string;
  };
  stop: { method: string; graceMs: string; timeoutMs: string };
  logs: {
    dir: string;
    rotation: string;
    maxFileMb: string;
    maxFiles: string;
    maxDays: string;
    encoding: string;
  };
  power: { resumeGraceMs: string };
};

/** 与后端 TaskDraft::default() 保持一致的新建默认值。 */
export function blankForm(): DraftForm {
  return {
    id: "",
    name: "",
    enabled: true,
    command: "",
    args: "",
    workingDir: "",
    autoStart: false,
    elevated: false,
    notify: true,
    ports: "",
    env: "",
    envProxy: "bypass_localhost",
    panelUrl: "",
    health: {
      kind: "process",
      intervalMs: "10000",
      timeoutMs: "2000",
      startupGraceMs: "20000",
      failureThreshold: "3",
      path: "",
      maxAgeMs: "",
      needsHumanStatus: "",
      url: "",
      expectStatus: "200",
      expectBodyContains: "",
    },
    restart: {
      mode: "on-failure",
      initialDelayMs: "3000",
      maxDelayMs: "60000",
      multiplier: "2",
      jitter: "0.2",
      maxConsecutiveFailures: "5",
      pauseOnExitCodes: "",
      stableAfterMs: "60000",
      windowMs: "600000",
      maxStartsInWindow: "6",
    },
    stop: { method: "graceful", graceMs: "2500", timeoutMs: "8000" },
    logs: {
      dir: "",
      rotation: "size",
      maxFileMb: "10",
      maxFiles: "5",
      maxDays: "7",
      encoding: "auto",
    },
    power: { resumeGraceMs: "" },
  };
}

/** 草稿阶段 → 中文短标签（未知阶段原样显示）。 */
export const draftStageLabel: Record<string, string> = {
  read: "读取",
  parse: "解析",
  conflict: "冲突",
  config: "配置",
  validate: "校验",
  write: "写入",
  join: "线程",
};

/** 保存/删除时磁盘 revision 已变化的统一提示。 */
export const CONFLICT_MESSAGE =
  "文件已被外部修改：当前草稿基于旧版本，未写入。请重新加载后再保存。";

/** 错误区的三个状态位；`null` 表示没有错误。 */
export interface EditorFeedback {
  error: string | null;
  detail: TaskDraftError | null;
  conflict: boolean;
}

export const CLEAR_FEEDBACK: EditorFeedback = {
  error: null,
  detail: null,
  conflict: false,
};

/** 判断任意错误值是否为草稿命令错误 DTO。 */
export function draftErrorOf(error: unknown): TaskDraftError | null {
  if (typeof error === "object" && error !== null && "stage" in error) {
    return error as TaskDraftError;
  }
  return null;
}

/** 保存/删除失败 → 错误区状态；冲突只提示并保留表单。 */
export function failureFeedback(failure: unknown): EditorFeedback {
  const detail = draftErrorOf(failure);
  if (detail?.stage === "conflict") {
    return { error: CONFLICT_MESSAGE, detail, conflict: true };
  }
  return { error: detail ? null : errorMessage(failure), detail, conflict: false };
}

/**
 * 写入前比对磁盘 revision：为空或一致时返回 null（继续写入），
 * 否则返回冲突反馈（调用方放弃写入并提示重新加载）。
 */
export function staleFeedback(
  revision: string,
  diskRevision: string,
): EditorFeedback | null {
  if (revision.length === 0 || revision === diskRevision) {
    return null;
  }
  return {
    error: CONFLICT_MESSAGE,
    detail: {
      stage: "conflict",
      message: "文件已被外部修改，请重新加载后再保存",
      code: null,
      taskId: null,
      currentRevision: diskRevision,
      line: null,
      col: null,
    },
    conflict: true,
  };
}

/** 保存/删除流程需要组件提供的最小能力集合（组件用 `$state` 赋值实现）。 */
export interface DraftWriteIo {
  readRevision(): Promise<string>;
  currentRevision(): string;
  setRevision(revision: string): void;
  setBusy(busy: boolean): void;
  apply(feedback: EditorFeedback): void;
  onSaved(result: TaskDraftResult): void;
  onFailure(): void;
}

/**
 * 读磁盘 revision → 冲突检测 → 写入 → 交回结果。
 *
 * 冲突时不写文件，只把冲突反馈交给组件；任何失败都走 `onFailure`，
 * 由组件决定是否关闭确认框（保存/删除共用）。
 */
export async function writeDraft(
  io: DraftWriteIo,
  write: (expectedRevision: string) => Promise<TaskDraftResult>,
): Promise<void> {
  io.setBusy(true);
  io.apply(CLEAR_FEEDBACK);
  try {
    const diskRevision = await io.readRevision();
    const patch = staleFeedback(io.currentRevision(), diskRevision);
    if (patch !== null) {
      io.apply(patch);
      return;
    }
    io.setRevision(diskRevision);
    io.onSaved(await write(diskRevision));
  } catch (failure) {
    io.apply(failureFeedback(failure));
    io.onFailure();
  } finally {
    io.setBusy(false);
  }
}

/** 空串 → null（表示不写这个键）。 */
export function optionalText(text: string): string | null {
  const value = text.trim();
  return value.length === 0 ? null : value;
}

export function splitLines(text: string): string[] {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
}

export function splitList(text: string): string[] {
  return text
    .split(",")
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
}

export function parseUint(text: string, label: string, min = 0): number {
  const value = text.trim();
  if (!/^\d+$/.test(value)) {
    throw new Error(label + " 必须是非负整数");
  }
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < min) {
    throw new Error(label + " 不能小于 " + min);
  }
  return parsed;
}

export function parseOptionalUint(
  text: string,
  label: string,
  min = 0,
): number | null {
  return text.trim().length === 0 ? null : parseUint(text, label, min);
}

export function parseRange(
  text: string,
  label: string,
  min: number,
  max: number,
): number {
  const value = parseUint(text, label, min);
  if (value > max) {
    throw new Error(label + " 不能大于 " + max);
  }
  return value;
}

export function parseFloatValue(text: string, label: string): number {
  const value = text.trim();
  const parsed = Number(value);
  if (value.length === 0 || !Number.isFinite(parsed)) {
    throw new Error(label + " 必须是数字");
  }
  return parsed;
}

export function parseIntList(text: string, label: string, min = 0): number[] {
  return splitList(text).map((item) => parseUint(item, label, min));
}

/** 逗号分隔的端口列表（端口从 1 起）。 */
export function parsePortList(text: string): number[] {
  return parseIntList(text, "ports", 1);
}

/** 逗号分隔的退出码列表（0 合法）。 */
export function parseExitCodes(text: string): number[] {
  return parseIntList(text, "restart.pause_on_exit_codes", 0);
}

/** 每行 KEY=VALUE；空行忽略，值里的 = 原样保留。 */
export function parseEnvText(text: string): Record<string, string> {
  const env: Record<string, string> = {};
  const lines = text.split("\n");
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index].trim();
    if (line.length === 0) {
      continue;
    }
    const separator = line.indexOf("=");
    if (separator <= 0) {
      throw new Error("env 第 " + (index + 1) + " 行不是 KEY=VALUE");
    }
    const key = line.slice(0, separator).trim();
    if (key.length === 0) {
      throw new Error("env 第 " + (index + 1) + " 行缺少变量名");
    }
    env[key] = line.slice(separator + 1).trim();
  }
  return env;
}

/** 数字或 null → 表单文本。 */
function optionalNumber(value: number | null): string {
  return value === null ? "" : String(value);
}

export function formOfDraft(draft: TaskDraft): DraftForm {
  return {
    id: draft.id,
    name: draft.name,
    enabled: draft.enabled,
    command: draft.command,
    args: draft.args.join("\n"),
    workingDir: draft.workingDir,
    autoStart: draft.autoStart,
    elevated: draft.elevated,
    notify: draft.notify,
    ports: draft.ports.join(", "),
    env: Object.entries(draft.env)
      .map(([key, value]) => key + "=" + value)
      .join("\n"),
    envProxy: draft.envProxy,
    panelUrl: draft.panelUrl ?? "",
    health: {
      kind: draft.health.kind,
      intervalMs: String(draft.health.intervalMs),
      timeoutMs: String(draft.health.timeoutMs),
      startupGraceMs: String(draft.health.startupGraceMs),
      failureThreshold: String(draft.health.failureThreshold),
      path: draft.health.path ?? "",
      maxAgeMs: optionalNumber(draft.health.maxAgeMs),
      needsHumanStatus: draft.health.needsHumanStatus.join(", "),
      url: draft.health.url ?? "",
      expectStatus: draft.health.expectStatus.join(", "),
      expectBodyContains: draft.health.expectBodyContains ?? "",
    },
    restart: {
      mode: draft.restart.mode,
      initialDelayMs: String(draft.restart.initialDelayMs),
      maxDelayMs: String(draft.restart.maxDelayMs),
      multiplier: String(draft.restart.multiplier),
      jitter: String(draft.restart.jitter),
      maxConsecutiveFailures: String(draft.restart.maxConsecutiveFailures),
      pauseOnExitCodes: draft.restart.pauseOnExitCodes.join(", "),
      stableAfterMs: String(draft.restart.stableAfterMs),
      windowMs: String(draft.restart.windowMs),
      maxStartsInWindow: String(draft.restart.maxStartsInWindow),
    },
    stop: {
      method: draft.stop.method,
      graceMs: String(draft.stop.graceMs),
      timeoutMs: String(draft.stop.timeoutMs),
    },
    logs: {
      dir: draft.logs.dir ?? "",
      rotation: draft.logs.rotation,
      maxFileMb: String(draft.logs.maxFileMb),
      maxFiles: String(draft.logs.maxFiles),
      maxDays: String(draft.logs.maxDays),
      encoding: draft.logs.encoding,
    },
    power: { resumeGraceMs: optionalNumber(draft.power.resumeGraceMs) },
  };
}

/** 表单 → 草稿；本地能判定的错误直接抛出可读消息。 */
export function draftOfForm(form: DraftForm): TaskDraft {
  const id = form.id.trim();
  if (id.length === 0) {
    throw new Error("id 不能为空");
  }
  if (form.workingDir.trim().length === 0) {
    throw new Error("工作目录不能为空");
  }
  if (form.command.trim().length === 0) {
    throw new Error("命令不能为空");
  }
  return {
    id,
    name: form.name,
    enabled: form.enabled,
    command: form.command,
    args: splitLines(form.args),
    workingDir: form.workingDir,
    autoStart: form.autoStart,
    elevated: form.elevated,
    notify: form.notify,
    env: parseEnvText(form.env),
    ports: parsePortList(form.ports),
    envProxy: form.envProxy,
    panelUrl: optionalText(form.panelUrl),
    health: {
      kind: form.health.kind,
      intervalMs: parseUint(form.health.intervalMs, "health.interval_ms", 1000),
      timeoutMs: parseUint(form.health.timeoutMs, "health.timeout_ms", 100),
      startupGraceMs: parseUint(
        form.health.startupGraceMs,
        "health.startup_grace_ms",
      ),
      failureThreshold: parseUint(
        form.health.failureThreshold,
        "health.failure_threshold",
        1,
      ),
      path: optionalText(form.health.path),
      maxAgeMs: parseOptionalUint(
        form.health.maxAgeMs,
        "health.max_age_ms",
        1000,
      ),
      needsHumanStatus: splitList(form.health.needsHumanStatus),
      url: optionalText(form.health.url),
      expectStatus: parseIntList(
        form.health.expectStatus,
        "health.expect_status",
        100,
      ),
      expectBodyContains: optionalText(form.health.expectBodyContains),
    },
    restart: {
      mode: form.restart.mode,
      initialDelayMs: parseUint(
        form.restart.initialDelayMs,
        "restart.initial_delay_ms",
      ),
      maxDelayMs: parseUint(form.restart.maxDelayMs, "restart.max_delay_ms"),
      multiplier: parseFloatValue(form.restart.multiplier, "restart.multiplier"),
      jitter: parseFloatValue(form.restart.jitter, "restart.jitter"),
      maxConsecutiveFailures: parseUint(
        form.restart.maxConsecutiveFailures,
        "restart.max_consecutive_failures",
      ),
      pauseOnExitCodes: parseExitCodes(form.restart.pauseOnExitCodes),
      stableAfterMs: parseUint(
        form.restart.stableAfterMs,
        "restart.stable_after_ms",
      ),
      windowMs: parseRange(
        form.restart.windowMs,
        "restart.window_ms",
        0,
        86_400_000,
      ),
      maxStartsInWindow: parseRange(
        form.restart.maxStartsInWindow,
        "restart.max_starts_in_window",
        0,
        1000,
      ),
    },
    stop: {
      method: form.stop.method,
      graceMs: parseRange(form.stop.graceMs, "stop.grace_ms", 100, 60000),
      timeoutMs: parseUint(form.stop.timeoutMs, "stop.timeout_ms"),
    },
    logs: {
      dir: optionalText(form.logs.dir),
      rotation: form.logs.rotation,
      maxFileMb: parseUint(form.logs.maxFileMb, "logs.max_file_mb"),
      maxFiles: parseUint(form.logs.maxFiles, "logs.max_files"),
      maxDays: parseUint(form.logs.maxDays, "logs.max_days"),
      encoding: form.logs.encoding,
    },
    power: {
      resumeGraceMs: parseOptionalUint(
        form.power.resumeGraceMs,
        "power.resume_grace_ms",
      ),
    },
  };
}
