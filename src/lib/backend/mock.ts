// 浏览器演示后端（mock）：让前端可以脱离 Rust/Tauri 独立开发与验收。
//
// 只在没有 Tauri 宿主时由 backend/index.ts 动态加载；Tauri 运行时永远不会
// 执行这里的代码。数据全部在内存里，刷新页面即重置；不会读写磁盘、不会启动进程。
//
// 它遵循与真实后端相同的契约（命令名、DTO 形状、事件名），因此组件/服务层不需要
// 为演示模式写任何分支。

import type {
  AutostartStatus,
  ConfigIssue,
  ConfigTextView,
  DiagnosticsResult,
  EventLogView,
  NotificationPayload,
  PluginImportResult,
  PluginInspectView,
  PluginSyncResult,
  RuntimeView,
  SaveConfigTextResult,
  SaveScheduleResult,
  ScheduleDraft,
  ScheduleEditorView,
  ScheduleView,
  Snapshot,
  SupervisorView,
  TaskCommandResult,
  TaskDraft,
  TaskDraftResult,
  TaskLogFilesView,
  TaskLogView,
  TaskSnapshot,
  ToastStatus,
  UnlistenFn,
} from "$lib/api";
import type { Backend } from "./index";

const MIN = 60_000;
const HOUR = 60 * MIN;
const NOW = Date.now();
const DATA_DIR = "C:/Users/demo/AppData/Local/Rook";
const CONFIG_PATH = "C:/Users/demo/AppData/Roaming/Rook/config.toml";
const MANIFEST = "rook.plugin.toml";

let revision = "9c41f0a2d7e8b3c6";
const listeners = new Set<(snapshot: Snapshot) => void>();
const notificationListeners = new Set<(payload: NotificationPayload) => void>();
const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/** 本地时间 -> YYYY-MM-DD HH:mm:ss。 */
function stamp(ms: number): string {
  const date = new Date(ms);
  const pad = (value: number) => String(value).padStart(2, "0");
  return (
    date.getFullYear() + "-" + pad(date.getMonth() + 1) + "-" + pad(date.getDate()) +
    " " + pad(date.getHours()) + ":" + pad(date.getMinutes()) + ":" + pad(date.getSeconds())
  );
}

/** 相对今天第 n 天的 hh:mm。 */
function atClock(dayOffset: number, hour: number, minute: number): number {
  const date = new Date();
  date.setDate(date.getDate() + dayOffset);
  date.setHours(hour, minute, 0, 0);
  return date.getTime();
}

function emptySchedule(): ScheduleView {
  return { enabled: false, windows: [], inWindow: false, nextStartAt: null, nextEndAt: null };
}

function runtime(patch: Partial<RuntimeView> = {}): RuntimeView {
  return {
    status: "stopped",
    pid: null,
    instanceId: null,
    startedAt: null,
    exitCode: null,
    exitReason: null,
    lastError: null,
    treeEmpty: null,
    forcedKill: false,
    logPath: "",
    logStats: {
      readBytes: 0, writtenBytes: 0, droppedBytes: 0, ioErrors: 0,
      readErrors: 0, rotations: 0, queueDrops: 0,
    },
    ...patch,
  };
}

function baseSupervisor(patch: Partial<SupervisorView> = {}): SupervisorView {
  return {
    desiredOverride: null,
    consecutiveFailures: 0,
    totalStarts: 0,
    totalRestarts: 0,
    pausedReason: null,
    lastStatus: "stopped",
    nextRetryAt: null,
    needsHuman: { active: false, source: null, message: null, since: null },
    restartPending: false,
    ...patch,
  };
}

function baseTask(patch: Partial<TaskSnapshot> & { id: string }): TaskSnapshot {
  return {
    name: patch.id,
    enabled: true,
    command: "node",
    args: [],
    workingDir: "D:/scripts",
    autoStart: false,
    elevated: false,
    notify: true,
    healthType: "process",
    ports: [],
    panelUrl: null,
    plugin: null,
    schedule: emptySchedule(),
    stop: { method: "graceful", graceMs: 2500, timeoutMs: 8000 },
    valid: true,
    issues: [],
    runtime: runtime(),
    supervisor: baseSupervisor(),
    displayStatus: "stopped",
    allowedActions: { start: true, stop: false, restart: true },
    ...patch,
  };
}

const scheduleWeekday: ScheduleView = {
  enabled: true,
  windows: [
    { days: ["mon", "tue", "wed", "thu", "fri"], start: "07:50", end: "12:10" },
    { days: ["mon", "tue", "wed", "thu", "fri"], start: "13:50", end: "18:10" },
  ],
  inWindow: false,
  nextStartAt: atClock(1, 7, 50),
  nextEndAt: atClock(1, 12, 10),
};

let tasks: TaskSnapshot[] = [
  baseTask({
    id: "ykt-gateway",
    name: "雨课堂网关",
    command: "node",
    args: ["gateway.js"],
    workingDir: "D:/scripts/ykt",
    autoStart: true,
    healthType: "http",
    ports: [3300],
    panelUrl: "http://127.0.0.1:3300/",
    plugin: {
      id: "ykt-gateway",
      manifestPath: "D:/scripts/ykt/" + MANIFEST,
      version: "0.2.0",
      updateAvailable: false,
    },
    schedule: scheduleWeekday,
    runtime: runtime({
      status: "running",
      pid: 24680,
      instanceId: "3f6a1c92b4d84e0a",
      startedAt: NOW - 2 * HOUR - 13 * MIN,
      logPath: DATA_DIR + "/logs/ykt-gateway/current.log",
      logStats: {
        readBytes: 1284511, writtenBytes: 1284511, droppedBytes: 0, ioErrors: 0,
        readErrors: 0, rotations: 1, queueDrops: 0,
      },
    }),
    supervisor: baseSupervisor({ lastStatus: "running", totalStarts: 12, totalRestarts: 1 }),
    displayStatus: "running",
    allowedActions: { start: false, stop: true, restart: true },
  }),
  baseTask({
    id: "zhs-gateway",
    name: "智慧树签到",
    command: "node",
    args: ["gateway.js", "--port", "3400"],
    workingDir: "D:/scripts/zhs",
    autoStart: true,
    healthType: "heartbeat",
    ports: [3400],
    plugin: {
      id: "zhs-gateway",
      manifestPath: "D:/scripts/zhs/" + MANIFEST,
      version: "0.1.5",
      updateAvailable: true,
    },
    runtime: runtime({
      status: "stopped",
      startedAt: NOW - 3 * HOUR - 20 * MIN,
      exitCode: 0,
      exitReason: "natural",
      treeEmpty: true,
      logPath: DATA_DIR + "/logs/zhs-gateway/current.log",
    }),
    supervisor: baseSupervisor({
      lastStatus: "paused",
      pausedReason: "needs_human",
      totalStarts: 6,
      totalRestarts: 2,
      needsHuman: {
        active: true,
        source: "heartbeat",
        message: "等待扫码登录",
        since: NOW - 20 * MIN,
      },
    }),
    displayStatus: "needs_human",
    allowedActions: { start: true, stop: false, restart: true },
  }),
  baseTask({
    id: "campus-net",
    name: "校园网自动重连",
    command: "python",
    args: ["reconnect.py"],
    workingDir: "D:/scripts/CampusNetwork",
    autoStart: true,
    healthType: "heartbeat",
    plugin: {
      id: "campus-net",
      manifestPath: "D:/scripts/CampusNetwork/" + MANIFEST,
      version: "0.3.1",
      updateAvailable: false,
    },
    runtime: runtime({
      status: "running",
      pid: 30912,
      instanceId: "aa71e4d0c9b24719",
      startedAt: NOW - 26 * MIN,
      logPath: DATA_DIR + "/logs/campus-net/current.log",
      logStats: {
        readBytes: 88210, writtenBytes: 88210, droppedBytes: 0, ioErrors: 0,
        readErrors: 0, rotations: 0, queueDrops: 0,
      },
    }),
    supervisor: baseSupervisor({ lastStatus: "running", totalStarts: 4, totalRestarts: 0 }),
    displayStatus: "running",
    allowedActions: { start: false, stop: true, restart: true },
  }),
  baseTask({
    id: "libseat-keeper",
    name: "图书馆座位看守",
    command: "node",
    args: ["keeper.js"],
    workingDir: "D:/scripts/libseat",
    autoStart: false,
    runtime: runtime({
      status: "stopped",
      startedAt: NOW - 6 * HOUR,
      exitCode: 1,
      exitReason: "natural",
      treeEmpty: true,
      logPath: DATA_DIR + "/logs/libseat-keeper/current.log",
    }),
    supervisor: baseSupervisor({
      lastStatus: "paused",
      pausedReason: "crash_loop",
      consecutiveFailures: 5,
      totalStarts: 9,
      totalRestarts: 8,
    }),
    displayStatus: "paused",
    allowedActions: { start: true, stop: false, restart: true },
  }),
];

const issues: ConfigIssue[] = [];

function cloneSnapshot(): Snapshot {
  return structuredClone(snapshot);
}

let snapshot: Snapshot = {
  appVersion: "0.1.0",
  configPath: CONFIG_PATH,
  configRevision: revision,
  dataDir: DATA_DIR,
  configError: null,
  issues,
  tasks,
  confirmExit: true,
  trayHintPending: false,
  configWatch: {
    enabled: true,
    state: "watching",
    lastReloadAt: NOW - 2 * HOUR,
    lastError: null,
  },
};

function emitSnapshot(): void {
  const next = cloneSnapshot();
  for (const listener of listeners) {
    listener(next);
  }
}

function emitNotification(payload: NotificationPayload): void {
  for (const listener of notificationListeners) {
    listener(payload);
  }
}

function findTask(taskId: string): TaskSnapshot {
  const task = snapshot.tasks.find((entry) => entry.id === taskId);
  if (!task) {
    throw {
      stage: "invalid",
      message: "没有这个任务：" + taskId,
      code: null,
      taskId,
      currentRevision: revision,
      line: null,
      col: null,
    };
  }
  return task;
}

function nextRevision(): string {
  return (Date.now().toString(16) + Math.random().toString(16).slice(2, 8)).slice(0, 16);
}

function replaceTask(next: TaskSnapshot): void {
  snapshot.tasks = snapshot.tasks.map((task) => (task.id === next.id ? next : task));
}

/** 只有配置写操作才改变 revision；启停/重启等运行时状态不改变配置文件。 */
function touchRevision(): void {
  revision = nextRevision();
  snapshot.configRevision = revision;
}

function commandResult(task: TaskSnapshot, action: "start" | "stop" | "restart"): TaskCommandResult {
  return {
    taskId: task.id,
    action,
    runtime: task.runtime,
    snapshot: cloneSnapshot(),
  };
}

function markBusy(taskId: string, status: "starting" | "stopping"): TaskSnapshot {
  const task = findTask(taskId);
  const next: TaskSnapshot = {
    ...task,
    displayStatus: status,
    runtime: { ...(task.runtime ?? runtime()), status },
    allowedActions: { start: false, stop: false, restart: false },
  };
  replaceTask(next);
  return next;
}

async function startTask(id: string): Promise<TaskCommandResult> {
  markBusy(id, "starting");
  emitSnapshot();
  await sleep(520);
  const task = findTask(id);
  const next: TaskSnapshot = {
    ...task,
    displayStatus: "running",
    runtime: {
      ...(task.runtime ?? runtime()),
      status: "running",
      pid: 10000 + Math.floor(Math.random() * 50000),
      instanceId: Math.random().toString(16).slice(2, 18),
      startedAt: Date.now(),
      exitCode: null,
      exitReason: null,
      lastError: null,
      forcedKill: false,
    },
    supervisor: {
      ...task.supervisor,
      lastStatus: "running",
      pausedReason: null,
      nextRetryAt: null,
      needsHuman: { active: false, source: null, message: null, since: null },
      totalStarts: task.supervisor.totalStarts + 1,
    },
    allowedActions: { start: false, stop: true, restart: true },
  };
  replaceTask(next);
  emitSnapshot();
  return commandResult(next, "start");
}

async function stopTask(id: string): Promise<TaskCommandResult> {
  markBusy(id, "stopping");
  emitSnapshot();
  await sleep(420);
  const task = findTask(id);
  const next: TaskSnapshot = {
    ...task,
    displayStatus: "stopped",
    runtime: {
      ...(task.runtime ?? runtime()),
      status: "stopped",
      pid: null,
      exitCode: 0,
      exitReason: "user_stop",
      treeEmpty: true,
      forcedKill: false,
    },
    supervisor: {
      ...task.supervisor,
      lastStatus: "stopped",
      needsHuman: { active: false, source: null, message: null, since: null },
    },
    allowedActions: { start: true, stop: false, restart: true },
  };
  replaceTask(next);
  emitSnapshot();
  return commandResult(next, "stop");
}

async function restartTask(id: string): Promise<TaskCommandResult> {
  await stopTask(id);
  return startTask(id);
}

// ---------- 任务草稿 ----------

function defaultDraft(): TaskDraft {
  return {
    id: "",
    name: "",
    enabled: true,
    command: "",
    args: [],
    workingDir: "",
    autoStart: false,
    elevated: false,
    notify: true,
    env: {},
    ports: [],
    envProxy: "bypass_localhost",
    panelUrl: null,
    health: {
      kind: "process",
      intervalMs: 10000,
      timeoutMs: 2000,
      startupGraceMs: 20000,
      failureThreshold: 3,
      path: null,
      maxAgeMs: null,
      needsHumanStatus: [],
      url: null,
      expectStatus: [200],
      expectBodyContains: null,
    },
    restart: {
      mode: "on-failure",
      initialDelayMs: 3000,
      maxDelayMs: 60000,
      multiplier: 2,
      jitter: 0.2,
      maxConsecutiveFailures: 5,
      pauseOnExitCodes: [],
      stableAfterMs: 60000,
      windowMs: 600000,
      maxStartsInWindow: 6,
    },
    stop: { method: "graceful", graceMs: 2500, timeoutMs: 8000 },
    logs: { dir: null, rotation: "size", maxFileMb: 10, maxFiles: 5, maxDays: 7, encoding: "auto" },
    power: { resumeGraceMs: null },
  };
}

function draftOf(task: TaskSnapshot): TaskDraft {
  const base = defaultDraft();
  return {
    ...base,
    id: task.id,
    name: task.name,
    enabled: task.enabled,
    command: task.command,
    args: [...task.args],
    workingDir: task.workingDir,
    autoStart: task.autoStart,
    elevated: task.elevated,
    notify: task.notify,
    ports: [...task.ports],
    panelUrl: task.panelUrl,
    health: {
      ...base.health,
      kind: task.healthType,
      url: task.healthType === "http" ? "http://127.0.0.1:" + (task.ports[0] ?? 3000) + "/health" : null,
      maxAgeMs: task.healthType === "heartbeat" ? 300000 : null,
      needsHumanStatus: task.healthType === "heartbeat" ? ["login_required"] : [],
    },
    stop: { ...task.stop },
  };
}

function writeDraftToConfig(draft: TaskDraft): TaskDraftResult {
  const existing = snapshot.tasks.find((task) => task.id === draft.id);
  if (existing) {
    const next: TaskSnapshot = {
      ...existing,
      name: draft.name || draft.id,
      enabled: draft.enabled,
      command: draft.command,
      args: [...draft.args],
      workingDir: draft.workingDir,
      autoStart: draft.autoStart,
      elevated: draft.elevated,
      notify: draft.notify,
      ports: [...draft.ports],
      panelUrl: draft.panelUrl,
      healthType: draft.health.kind as TaskSnapshot["healthType"],
      stop: { ...draft.stop, method: draft.stop.method as TaskSnapshot["stop"]["method"] },
      supervisor: { ...existing.supervisor, restartPending: existing.displayStatus === "running" },
    };
    replaceTask(next);
  } else {
    const created = baseTask({
      id: draft.id,
      name: draft.name || draft.id,
      enabled: draft.enabled,
      command: draft.command,
      args: [...draft.args],
      workingDir: draft.workingDir,
      autoStart: draft.autoStart,
      elevated: draft.elevated,
      notify: draft.notify,
      ports: [...draft.ports],
      panelUrl: draft.panelUrl,
      healthType: draft.health.kind as TaskSnapshot["healthType"],
      stop: { ...draft.stop, method: draft.stop.method as TaskSnapshot["stop"]["method"] },
    });
    snapshot.tasks = [...snapshot.tasks, created];
  }
  touchRevision();
  emitSnapshot();
  return { newRevision: revision, changed: true, snapshot: cloneSnapshot() };
}

// ---------- 运行时段 ----------

const scheduleDrafts = new Map<string, ScheduleDraft>();

function scheduleDraftOf(task: TaskSnapshot): ScheduleDraft {
  return {
    enabled: task.schedule.enabled,
    windows: task.schedule.windows.map((window) => ({
      days: [...window.days],
      start: window.start,
      end: window.end,
    })),
  };
}

// ---------- 日志 ----------

const logLines = new Map<string, string[]>();

const logTemplates: Record<string, string[]> = {
  "ykt-gateway": [
    "[sentry] 网关已就绪，监听 127.0.0.1:3300",
    '[sentry] {"event":"heartbeat","status":"ok","course":"数据结构"}',
    "[sentry] 已连接课堂通道，等待教师发起签到",
    "GET /api/state 200 4ms",
    "[sentry] 页面状态：已进入课堂",
    "WebSocket 客户端已连接（127.0.0.1）",
  ],
  "zhs-gateway": [
    "[sentry] 读取本地会话 Cookie",
    "[sentry] 登录状态：等待扫码",
    '[sentry] {"event":"needs_human","reason":"login_required"}',
  ],
  "campus-net": [
    "[net] 检查网络连通性 …",
    "[net] 探测到 portal 拦截，准备重连",
    "[net] 认证成功，出口已恢复",
    "[net] 心跳已写入",
  ],
  "libseat-keeper": [
    "[seat] 打开图书馆预约页",
    "[seat] 选择座位 3F-042",
    "[seat] 预约失败：当前时段已满",
    "[seat] 退出码 1",
  ],
};

function seededLogs(taskId: string, count: number): string[] {
  const templates = logTemplates[taskId] ?? ["[sentry] 应用日志已就绪"];
  const lines: string[] = [];
  for (let index = 0; index < count; index += 1) {
    lines.push(stamp(NOW - (count - index) * 8000) + "  " + templates[index % templates.length]);
  }
  return lines;
}

for (const task of tasks) {
  logLines.set(task.id, seededLogs(task.id, 120));
}

setInterval(() => {
  for (const task of snapshot.tasks) {
    if (task.displayStatus !== "running") continue;
    const lines = logLines.get(task.id);
    if (!lines) continue;
    const templates = logTemplates[task.id] ?? ["[sentry] …"];
    lines.push(stamp(Date.now()) + "  " + templates[lines.length % templates.length]);
    if (lines.length > 400) lines.splice(0, lines.length - 400);
  }
}, 2000);

function archiveFiles(taskId: string) {
  const lines = logLines.get(taskId) ?? [];
  const bytes = lines.reduce((total, line) => total + line.length + 1, 0);
  return [
    { id: "current.log.1", size: Math.max(4096, bytes), modifiedAt: NOW - 26 * HOUR },
    { id: "current.log.2026-11-13", size: 1204224, modifiedAt: NOW - 2 * 24 * HOUR },
  ];
}

// ---------- 事件日志 ----------

const eventLines: string[] = (() => {
  const events: [string, string, Record<string, string>][] = [
    ["app.start", "Rook 启动", { version: "0.1.0" }],
    ["config.loaded", "配置加载完成", { tasks: "4" }],
    ["schedule.window_enter", "进入运行时段", { task: "ykt-gateway" }],
    ["task.start", "启动脚本", { task: "ykt-gateway", reason: "schedule_window" }],
    ["task.start", "进程已拉起", { task: "campus-net", reason: "auto_start" }],
    ["health.probe", "HTTP 健康探测通过", { task: "ykt-gateway", status: "200" }],
    ["task.pause", "等待人工处理", { task: "zhs-gateway", reason: "needs_human" }],
    ["notification.raised", "已生成应用内通知", { kind: "needs_human" }],
    ["restart.backoff", "等待退避后重启", { task: "campus-net", attempt: "2" }],
    ["task.exit", "进程退出", { task: "libseat-keeper", code: "1" }],
    ["task.pause", "连续失败，自动重启已暂停", { task: "libseat-keeper", reason: "crash_loop" }],
    ["config.watch", "配置文件未变化", { state: "watching" }],
  ];
  const out: string[] = [];
  for (let index = 0; index < 42; index += 1) {
    const [event, message, fields] = events[index % events.length];
    const ts = new Date(NOW - (42 - index) * 5 * MIN).toISOString();
    out.push(
      JSON.stringify({
        ts,
        level: event.startsWith("restart") || event === "task.pause" ? "WARN" : "INFO",
        event,
        message,
        fields,
      }),
    );
  }
  return out;
})();

// ---------- 配置文本 ----------

const configText = [
  "# Rook 配置文件（演示模式，不会写盘）",
  "[app]",
  "confirm_exit = true",
  "",
  "[supervisor]",
  "watch_config = true",
  "",
  "[[tasks]]",
  'id = "ykt-gateway"',
  'name = "雨课堂网关"',
  'command = "node"',
  'args = ["gateway.js"]',
  'working_dir = "D:/scripts/ykt"',
  "auto_start = true",
  "",
  "[tasks.health]",
  'type = "http"',
  'url = "http://127.0.0.1:3300/health"',
  "",
  "[tasks.schedule]",
  "enabled = true",
].join(String.fromCharCode(10));

// ---------- 通知 ----------

let notificationTimers: ReturnType<typeof setTimeout>[] = [];

function scheduleDemoNotifications(): void {
  if (notificationTimers.length > 0) return;
  notificationTimers = [
    setTimeout(() => {
      emitNotification({
        kind: "needs_human",
        level: "medium",
        title: "智慧树签到等待人工处理",
        message: "heartbeat status=login_required：扫描二维码后自动恢复监督",
        taskId: "zhs-gateway",
        taskName: "智慧树签到",
        count: 1,
        at: Date.now(),
      });
    }, 3500),
    setTimeout(() => {
      emitNotification({
        kind: "crash_loop",
        level: "high",
        title: "图书馆座位看守反复失败，已暂停",
        message: "连续失败 5 次，自动重启已关闭",
        taskId: "libseat-keeper",
        taskName: "图书馆座位看守",
        count: 1,
        at: Date.now(),
      });
    }, 7000),
  ];
}

const configWatch = {
  enabled: true,
  state: "watching" as const,
  lastReloadAt: NOW - 2 * HOUR,
  lastError: null,
};

const autostart: AutostartStatus = {
  enabled: true,
  mode: "run_key",
  expectedValue: '"D:/apps/Rook/Rook.exe" --autostart',
  actualValue: '"D:/apps/Rook/Rook.exe" --autostart',
  matches: true,
  registered: true,
  error: null,
};

function toastStatus(): ToastStatus {
  return {
    available: true,
    reason: null,
    aumid: "D:/apps/Rook/Rook.exe",
    shortcut: "C:/Users/demo/AppData/Roaming/Microsoft/Windows/Start Menu/Programs/Rook.lnk",
    lastError: null,
  };
}

// ---------- 传输实现 ----------

export const mockBackend: Backend = {
  async getSnapshot() {
    return cloneSnapshot();
  },
  async reloadConfig() {
    snapshot.configRevision = revision;
    snapshot.configWatch = { ...configWatch, lastReloadAt: Date.now() };
    emitSnapshot();
    return cloneSnapshot();
  },
  async quitApp() {
    console.info("[mock] quit_app");
  },
  async acknowledgeTrayHint() {
    snapshot.trayHintPending = false;
    return cloneSnapshot();
  },
  async getAutostartStatus() {
    return { ...autostart };
  },
  async getToastStatus() {
    return toastStatus();
  },
  async testToast() {
    return toastStatus();
  },

  startTask,
  stopTask,
  restartTask,

  async readTaskLog(taskId, query = {}) {
    findTask(taskId);
    const fileId = query.fileId ?? "current.log";
    const limit = Math.min(query.tailLines ?? 200, 2000);
    const all = fileId === "current.log" ? (logLines.get(taskId) ?? []) : (logLines.get(taskId) ?? []).slice(0, 40);
    const lines = all.slice(-limit);
    const archive = archiveFiles(taskId).find((file) => file.id === fileId);
    return {
      taskId,
      fileId,
      path: DATA_DIR + "/logs/" + taskId + "/" + fileId,
      lines,
      truncated: all.length > limit,
      totalBytes: archive?.size ?? all.reduce((total, line) => total + line.length + 1, 0),
      modifiedAt: archive?.modifiedAt ?? Date.now(),
      warning: null,
    } satisfies TaskLogView;
  },
  async listTaskLogs(taskId) {
    findTask(taskId);
    const archives = archiveFiles(taskId);
    const currentSize = archives[0]?.size ?? 0;
    return {
      taskId,
      currentFileId: "current.log",
      files: [
        { id: "current.log", name: "current.log", size: currentSize, modifiedAt: Date.now(), current: true },
        ...archives.map((file) => ({
          id: file.id,
          name: file.id,
          size: file.size,
          modifiedAt: file.modifiedAt,
          current: false,
        })),
      ],
    } satisfies TaskLogFilesView;
  },

  async readConfigText() {
    return { path: CONFIG_PATH, text: configText, revision } satisfies ConfigTextView;
  },
  async saveConfigText(text, expectedRevision) {
    if (expectedRevision !== revision) {
      throw {
        stage: "conflict",
        message: "文件已被外部修改，未写入",
        code: null,
        taskId: null,
        currentRevision: revision,
        line: null,
        col: null,
      };
    }
    revision = nextRevision();
    snapshot.configRevision = revision;
    emitSnapshot();
    return { revision, changed: text !== configText, snapshot: cloneSnapshot() } satisfies SaveConfigTextResult;
  },
  async restoreConfigBackup() {
    revision = nextRevision();
    snapshot.configRevision = revision;
    emitSnapshot();
    return { revision, changed: true, snapshot: cloneSnapshot() } satisfies SaveConfigTextResult;
  },

  async getTaskDefaults() {
    return defaultDraft();
  },
  async getTaskDraft(taskId) {
    return draftOf(findTask(taskId));
  },
  async saveTaskDraft(draft, expectedRevision) {
    if (expectedRevision !== revision) {
      throw {
        stage: "conflict",
        message: "文件已被外部修改，未写入",
        code: null,
        taskId: draft.id,
        currentRevision: revision,
        line: null,
        col: null,
      };
    }
    return writeDraftToConfig(draft);
  },
  async deleteTask(taskId, expectedRevision) {
    if (expectedRevision !== revision) {
      throw {
        stage: "conflict",
        message: "文件已被外部修改，未写入",
        code: null,
        taskId,
        currentRevision: revision,
        line: null,
        col: null,
      };
    }
    snapshot.tasks = snapshot.tasks.filter((task) => task.id !== taskId);
    touchRevision();
    emitSnapshot();
    return { newRevision: revision, changed: true, snapshot: cloneSnapshot() } satisfies TaskDraftResult;
  },
  async reorderTasks(ids, expectedRevision) {
    if (expectedRevision !== revision) {
      throw {
        stage: "conflict",
        message: "文件已被外部修改，未写入",
        code: null,
        taskId: null,
        currentRevision: revision,
        line: null,
        col: null,
      };
    }
    const known = snapshot.tasks.map((task) => task.id);
    if (ids.length !== known.length || ids.some((id) => !known.includes(id))) {
      throw {
        stage: "invalid",
        message: "任务集合不一致",
        code: null,
        taskId: null,
        currentRevision: revision,
        line: null,
        col: null,
      };
    }
    snapshot.tasks = ids.map((id) => findTask(id));
    touchRevision();
    emitSnapshot();
    return { newRevision: revision, changed: true, snapshot: cloneSnapshot() } satisfies TaskDraftResult;
  },

  async getSchedule(taskId) {
    const task = findTask(taskId);
    return {
      taskId,
      schedule: scheduleDrafts.get(taskId) ?? scheduleDraftOf(task),
      revision,
    } satisfies ScheduleEditorView;
  },
  async saveSchedule(taskId, draft, expectedRevision) {
    if (expectedRevision !== revision) {
      throw {
        stage: "conflict",
        message: "文件已被外部修改，未写入",
        code: null,
        taskId,
        currentRevision: revision,
        line: null,
        col: null,
      };
    }
    // draft 可能是 Svelte 的响应式代理：这里手动拷成普通对象，避免 structuredClone 失败。
    scheduleDrafts.set(taskId, {
      enabled: draft.enabled,
      windows: draft.windows.map((window) => ({
        days: [...window.days],
        start: window.start,
        end: window.end,
      })),
    });
    const task = findTask(taskId);
    replaceTask({
      ...task,
      schedule: {
        enabled: draft.enabled,
        windows: draft.windows.map((window) => ({
          days: [...window.days],
          start: window.start,
          end: window.end,
        })),
        inWindow: false,
        nextStartAt: draft.enabled ? atClock(1, 7, 50) : null,
        nextEndAt: draft.enabled ? atClock(1, 12, 10) : null,
      },
    });
    touchRevision();
    emitSnapshot();
    return { revision, changed: true, snapshot: cloneSnapshot() } satisfies SaveScheduleResult;
  },

  async inspectPlugin(manifestPath) {
    const base = manifestPath.replace(/[\/]+$/, "");
    const path = base.toLowerCase().endsWith(".toml") ? base : base + "/" + MANIFEST;
    return {
      manifestPath: path,
      rootDir: base,
      pluginId: "demo-gateway",
      name: "演示网关",
      version: "0.1.0",
      description: "用于界面演示的脚本清单，不会执行任何脚本。",
      homepage: null,
      command: "node",
      args: ["gateway.js"],
      workingDir: base,
      ports: [3100],
      env: { DEMO_MODE: "1" },
      envProxy: "bypass_localhost",
      panelUrl: null,
      healthType: "process",
      healthUrl: null,
      schedule: { enabled: false, windows: [] },
      taskId: "demo-gateway",
      existingTaskId: null,
      warnings: ["演示模式：导入只写入内存，刷新页面即消失。"],
      configRevision: revision,
    } satisfies PluginInspectView;
  },
  async importPlugin(manifestPath, expectedRevision) {
    if (expectedRevision !== revision) {
      throw {
        stage: "conflict",
        message: "文件已被外部修改，未写入",
        code: null,
        taskId: null,
        currentRevision: revision,
        line: null,
        col: null,
      };
    }
    const created = baseTask({
      id: "demo-gateway",
      name: "演示网关",
      command: "node",
      args: ["gateway.js"],
      workingDir: manifestPath.replace(/[\/][^\/]+$/, ""),
      ports: [3100],
      plugin: {
        id: "demo-gateway",
        manifestPath,
        version: "0.1.0",
        updateAvailable: false,
      },
    });
    snapshot.tasks = [...snapshot.tasks, created];
    touchRevision();
    emitSnapshot();
    return { taskId: created.id, snapshot: cloneSnapshot() } satisfies PluginImportResult;
  },
  async syncPlugin(taskId, expectedRevision) {
    if (expectedRevision !== revision) {
      throw {
        stage: "conflict",
        message: "文件已被外部修改，未写入",
        code: null,
        taskId,
        currentRevision: revision,
        line: null,
        col: null,
      };
    }
    const task = findTask(taskId);
    if (task.plugin) {
      replaceTask({ ...task, plugin: { ...task.plugin, updateAvailable: false, version: "0.2.0" } });
      touchRevision();
      emitSnapshot();
    }
    return { taskId, changed: true, snapshot: cloneSnapshot() } satisfies PluginSyncResult;
  },

  async openTaskPanel() {
    console.info("[mock] open_task_panel");
  },
  async openAppPath() {
    console.info("[mock] open_app_path");
  },
  async readEventLog(tailLines = 200) {
    const lines = eventLines.slice(-Math.min(tailLines, eventLines.length));
    return {
      path: DATA_DIR + "/core/events.jsonl",
      lines,
      truncated: eventLines.length > lines.length,
      totalBytes: eventLines.reduce((total, line) => total + line.length + 1, 0),
    } satisfies EventLogView;
  },
  async exportDiagnostics() {
    return {
      path: DATA_DIR + "/diagnostics/2026-11-15T20-30-00",
      files: 17,
    } satisfies DiagnosticsResult;
  },

  async onSnapshotChanged(handler) {
    listeners.add(handler);
    return (() => listeners.delete(handler)) satisfies UnlistenFn;
  },
  async onNotificationRaised(handler) {
    notificationListeners.add(handler);
    scheduleDemoNotifications();
    return (() => notificationListeners.delete(handler)) satisfies UnlistenFn;
  },
};
