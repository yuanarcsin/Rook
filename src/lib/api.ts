// 与后端快照/命令的手写契约（M1-4 → M2-4b）。
//
// 类型与 src-tauri/src/snapshot.rs 的 serde camelCase 输出一一对应。
// 这里不引入额外依赖，只封装 invoke 与事件订阅。

import type { UnlistenFn } from "@tauri-apps/api/event";
import { getTransport } from "$lib/backend";

export type Severity = "error" | "warning";

export interface ConfigIssue {
  severity: Severity;
  taskId: string | null;
  path: string;
  message: string;
  line: number | null;
  col: number | null;
}

export interface ConfigErrorView {
  file: string;
  line: number;
  col: number;
  path: string;
  message: string;
}

/** 任务运行状态：`stopped → starting → running → stopping → stopped`。 */
export type TaskStatus = "stopped" | "starting" | "running" | "stopping";

/** 本次实例结束的原因。 */
export type ExitReason = "natural" | "user_stop";

/** 运行时错误阶段（稳定英文标识）。 */
export type RuntimeStage =
  | "port_check"
  | "spawn"
  | "logs"
  | "watch"
  | "stop"
  | "invalid_state";

export interface RuntimeErrorView {
  stage: RuntimeStage;
  /** 底层 Win32 错误码；没有系统码时为 null。 */
  code: number | null;
  message: string;
}

export interface LogStatsView {
  readBytes: number;
  writtenBytes: number;
  droppedBytes: number;
  ioErrors: number;
  readErrors: number;
  rotations: number;
  queueDrops: number;
}

/** 任务运行时视图（`TaskSnapshot.runtime`）。 */
export interface RuntimeView {
  status: TaskStatus;
  pid: number | null;
  instanceId: string | null;
  /** 最近一次成功启动时间，Unix epoch 毫秒。 */
  startedAt: number | null;
  exitCode: number | null;
  exitReason: ExitReason | null;
  lastError: RuntimeErrorView | null;
  treeEmpty: boolean | null;
  forcedKill: boolean;
  logPath: string;
  logStats: LogStatsView;
}

/** state.json 恢复出的 `needs_human` 标志（M3-4a）。 */
export interface NeedsHumanView {
  active: boolean;
  source: string | null;
  message: string | null;
  /** 置位时刻，Unix epoch 毫秒。 */
  since: number | null;
}

/**
 * 暂停原因（与后端 `PausedReason::as_str()` 一致）。
 *
 * 后端还有 `Other(text)` 分支，会把未知取值原样返回；前端不枚举它，
 * 展示层按字符串兜底（`pausedReasonLabel` 未知值原样显示）。
 */
export type PausedReason =
  | "crash_loop"
  | "restart_budget"
  | "needs_human"
  | "crash_no_restart"
  | "user_pause"
  | "elevation_unavailable";

/** 监督状态（与后端 `SupervisorStatus::as_str()` 一致）。 */
export type SupervisorStatus =
  | "stopped"
  | "starting"
  | "running"
  | "stopping"
  | "backoff"
  | "paused";

/** 停止方式（与后端 `StopMethod::as_str()` 一致）。 */
export type StopMethod = "graceful" | "terminate";

/** 健康检查类型（与后端 `HealthType::as_str()` 一致）。 */
export type HealthType = "process" | "heartbeat" | "http";

/** 配置监听状态（与后端 `ConfigWatchState::as_str()` 一致）。 */
export type ConfigWatchState = "disabled" | "watching" | "polling" | "error";

/** 从 state.json 恢复的监督状态（M3-4a）；首次运行/文件缺失时为默认值。 */
export interface SupervisorView {
  desiredOverride: "stopped" | "paused" | null;
  consecutiveFailures: number;
  totalStarts: number;
  totalRestarts: number;
  /** `PausedReason::as_str()`；未暂停时为 null。 */
  pausedReason: PausedReason | null;
  /** 最近一次观察到的监督状态（`SupervisorStatus::as_str()`）。 */
  lastStatus: SupervisorStatus | null;
  /** 下次退避重试时刻，Unix epoch 毫秒；本里程碑只展示、不调度。 */
  nextRetryAt: number | null;
  needsHuman: NeedsHumanView;
  /**
   * M3-6a：配置已修改但进程仍在用旧配置（`apply_changes=next_start`），
   * 下一次"重启"后生效。
   */
  restartPending: boolean;
}

export interface TaskSnapshot {
  id: string;
  name: string;
  enabled: boolean;
  command: string;
  args: string[];
  workingDir: string;
  autoStart: boolean;
  elevated: boolean;
  notify: boolean;
  /** 健康检查类型（`HealthType::as_str()`）。 */
  healthType: HealthType;
  ports: number[];
  panelUrl: string | null;
  /** 插件来源（清单导入的任务）；手工任务为 null。 */
  plugin: PluginView | null;
  /** 每周运行时段（2026-11-04）：静态窗口 + 当前是否在窗内/下次起止。 */
  schedule: ScheduleView;
  /** 停止方式（2026-11-05）：graceful/terminate、宽限与总预算。 */
  stop: StopView;
  valid: boolean;
  issues: ConfigIssue[];
  /** 运行时状态；配置结构错误或未创建实例时为 null。 */
  runtime: RuntimeView | null;
  /** 从 state.json 恢复的监督状态（用户意图/失败预算/paused/needs_human）。 */
  supervisor: SupervisorView;
  /**
   * 后端按权威规则算出的展示状态：
   * `invalid` / `unknown` / `starting` / `running` / `stopping` / `stopped` /
   * `paused` / `needs_human` / `backoff` / `disabled`。
   */
  displayStatus: string;
  /** 后端按权威规则算出的可执行动作；界面不要自行推导。 */
  allowedActions: AllowedActions;
}

/** 任务可执行动作（`TaskSnapshot.allowedActions`）。 */
export interface AllowedActions {
  start: boolean;
  stop: boolean;
  restart: boolean;
}

/** 任务停止方式视图。 */
export interface StopView {
  /** `StopMethod::as_str()`：`"graceful"` / `"terminate"`。 */
  method: StopMethod;
  /** 优雅停止宽限（毫秒）。 */
  graceMs: number;
  /** 停止总预算（毫秒）。 */
  timeoutMs: number;
}

/** 配置监听错误（文件/行/列 + 说明）。 */
export interface ConfigWatchErrorView {
  file: string;
  line: number;
  col: number;
  message: string;
}

/** 配置热重载监听状态（`Snapshot.configWatch`）。 */
export interface ConfigWatchView {
  /** 运行时是否启用了监听（`[supervisor].watch_config`）。 */
  enabled: boolean;
  /** `ConfigWatchState::as_str()`：`"disabled"` / `"watching"` / `"polling"` / `"error"`。 */
  state: ConfigWatchState;
  /** 最近一次成功重载的时刻，Unix epoch 毫秒。 */
  lastReloadAt: number | null;
  /** 最近一次失败；成功重载后为 null。 */
  lastError: ConfigWatchErrorView | null;
}

export interface Snapshot {
  appVersion: string;
  configPath: string;
  /**
   * 当前配置内容 revision（与 `read_config_text`/保存命令同一套算法）。
   *
   * 配置不可用（结构/IO 错误）时为空串；保存草稿/时段可直接带上它做冲突检测，
   * 无需先读整份 TOML。
   */
  configRevision: string;
  dataDir: string | null;
  configError: ConfigErrorView | null;
  issues: ConfigIssue[];
  tasks: TaskSnapshot[];
  /** `[app].confirm_exit`：退出前是否弹确认（配置不可用时为 true）。 */
  confirmExit: boolean;
  /** 下次打开窗口是否展示一次“仍在托盘运行”的提示。 */
  trayHintPending: boolean;
  /** 配置热重载监听状态。 */
  configWatch: ConfigWatchView;
}

/** 通知级别：`high` 需要处理 / `medium` 需要人工处理（docs/05 §8）。 */
export type NotificationLevel = "high" | "medium";

/**
 * 通知类型：后端 `NotificationKind::as_str()` 的取值；
 * `config_warning` 是前端从快照里的配置 warning 派生的一条汇总通知（只进通知中心）。
 */
export type NotificationKind =
  | "needs_human"
  | "crash_loop"
  | "restart_budget"
  | "start_failed"
  | "stop_timeout"
  | "config_error"
  /** 前端从快照的配置 warning 派生（后端不单独发事件），只进通知中心。 */
  | "config_warning";

/**
 * 应用内通知（`notification:raised`，M3-5b）。
 *
 * 无系统 Toast 时后端只发这个事件：窗口打开时渲染 banner，窗口关闭时无人
 * 接收，用户靠托盘颜色/tooltip 判断；后端绝不为此打开或聚焦窗口。
 */
export interface NotificationPayload {
  kind: NotificationKind;
  level: NotificationLevel;
  title: string;
  message: string;
  /** 任务级通知的任务 id；配置错误为 null。 */
  taskId: string | null;
  taskName: string | null;
  /** 合并摘要覆盖的任务数；单条为 1。 */
  count: number;
  /** 生成时刻，Unix epoch 毫秒。 */
  at: number;
}

/** 启停命令的返回值。 */
export interface TaskCommandResult {
  taskId: string;
  action: "start" | "stop" | "restart";
  /** 动作完成后的运行时视图。 */
  runtime: RuntimeView | null;
  /** 动作完成后的完整快照，与 `snapshot:changed` 内容一致。 */
  snapshot: Snapshot;
}

/**
 * 统一 IPC 错误 DTO（A4）：所有命令失败时 reject 的形状。
 *
 * 与 `src-tauri/src/ipc_error.rs` 的 `IpcError` 一一对应：7 个字段恒定存在，
 * 不适用的字段为 `null`。`stage` 的取值见各命令自身的文档。
 */
export interface IpcError {
  /** 稳定英文阶段标识（如 `config` / `parse` / `conflict` / `join`）。 */
  stage: string;
  /** 面向用户的说明。 */
  message: string;
  /** 底层系统错误码；没有系统码时为 null。 */
  code: number | null;
  /** 相关任务 id（如果有）。 */
  taskId: string | null;
  /** 冲突时磁盘上的最新 revision；其他阶段为 null。 */
  currentRevision: string | null;
  /** 结构错误的行（1 起）；其他阶段为 null。 */
  line: number | null;
  /** 结构错误的列（1 起）；其他阶段为 null。 */
  col: number | null;
}

/** 启停命令失败时的错误 DTO（形状统一为 {@link IpcError}）。 */
export type CommandError = IpcError;

/** 日志尾部读取结果（M4-1，`read_task_log`）。 */
export interface TaskLogView {
  taskId: string;
  /** 实际读取的文件名：`current.log` 或轮转归档名。 */
  fileId: string;
  /** 实际读取的文件绝对路径（来自配置 + 文件名白名单）。 */
  path: string;
  /** 按文件顺序的最后若干行（不含换行符）。 */
  lines: string[];
  /** 是否因行数/字节上限只返回了尾部。 */
  truncated: boolean;
  /** 文件总字节数。 */
  totalBytes: number;
  /** 文件修改时间，Unix epoch 毫秒；取不到时为 null。 */
  modifiedAt: number | null;
  /** 解码提示（GBK 等非 UTF-8 输出按替换字符显示）；正常为 null。 */
  warning: string | null;
}

/** 日志读取失败的错误 DTO（形状统一为 {@link IpcError}）。 */
export type TaskLogError = IpcError;

/** 日志尾部读取参数：缺省 200 行、最多 256KB；后端会再次钳制。 */
export interface TaskLogQuery {
  tailLines?: number;
  maxBytes?: number;
  /** 文件名：缺省/null 为 `current.log`，其他只允许任务目录下的归档名。 */
  fileId?: string | null;
}

/** 任务日志目录中的一个可浏览文件（C3，`list_task_logs`）。 */
export interface LogFileView {
  /** 传给 `readTaskLog` 的 `fileId`（文件名，不含目录）。 */
  id: string;
  /** 下拉框显示名（当前即文件名）。 */
  name: string;
  size: number;
  /** 文件修改时间，Unix epoch 毫秒；取不到时为 null。 */
  modifiedAt: number | null;
  /** 是否是当前活动文件（`current.log`）。 */
  current: boolean;
}

/** 任务日志文件列表（C3，`list_task_logs`）。 */
export interface TaskLogFilesView {
  taskId: string;
  /** `current.log` 在前，归档按修改时间倒序（最多 50 个）。 */
  files: LogFileView[];
  /** 默认查看的文件，恒为 `current.log`。 */
  currentFileId: string;
}

/** 配置文本（M4-2，`read_config_text`）。 */
export interface ConfigTextView {
  /** 实际读取的配置文件路径（由后端按配置解析，前端不能指定）。 */
  path: string;
  text: string;
  /** 内容 revision（SHA-256 前 16 字节 hex），保存时必须原样带回。 */
  revision: string;
}

/** 保存配置文本的结果（`save_config_text`）。 */
export interface SaveConfigTextResult {
  /** 落盘后的 revision；内容未变时与传入的 expectedRevision 相同。 */
  revision: string;
  /** false = 内容与磁盘相同，未写盘、未触发重载。 */
  changed: boolean;
  /** 重新加载后的完整快照。 */
  snapshot: Snapshot;
}

/** 配置文本命令失败时的错误 DTO（形状统一为 {@link IpcError}）。 */
export type ConfigTextError = IpcError;

/**
 * 结构化任务草稿（M4-4a，`get_task_draft` / `save_task_draft`）。
 *
 * 与 `src-tauri/src/config/task_draft.rs` 的 serde camelCase 输出一一对应：
 * 缺省字段由后端填文档默认值，枚举一律用字符串承载，非法取值由后端校验拒绝。
 */
export interface TaskDraft {
  /** 任务 id（必填，编辑时不可变；找不到现有任务则新建）。 */
  id: string;
  name: string;
  enabled: boolean;
  command: string;
  args: string[];
  workingDir: string;
  autoStart: boolean;
  elevated: boolean;
  notify: boolean;
  env: Record<string, string>;
  ports: number[];
  envProxy: string;
  /** `null` / 空串表示不写该键。 */
  panelUrl: string | null;
  health: TaskDraftHealth;
  restart: TaskDraftRestart;
  stop: TaskDraftStop;
  logs: TaskDraftLogs;
  power: TaskDraftPower;
}

/** `[tasks.health]` */
export interface TaskDraftHealth {
  /** TOML 键名是 `type`，草稿里叫 `kind`：`process` / `http` / `heartbeat`。 */
  kind: string;
  intervalMs: number;
  timeoutMs: number;
  startupGraceMs: number;
  failureThreshold: number;
  /** 心跳文件路径；`null` 表示用 `<data>/heartbeats/<id>.hb`。 */
  path: string | null;
  /** 心跳最大年龄；`null` 表示不写（heartbeat 模式必填）。 */
  maxAgeMs: number | null;
  needsHumanStatus: string[];
  /** 仅 http 模式；`null` 表示不写该键。 */
  url: string | null;
  expectStatus: number[];
  expectBodyContains: string | null;
}

/** `[tasks.restart]` */
export interface TaskDraftRestart {
  mode: string;
  initialDelayMs: number;
  maxDelayMs: number;
  multiplier: number;
  jitter: number;
  maxConsecutiveFailures: number;
  pauseOnExitCodes: number[];
  stableAfterMs: number;
  /** 启动窗口长度（毫秒，0 = 不限）；窗口内启动次数超限会暂停自动重启。 */
  windowMs: number;
  /** 窗口内最多启动次数（0 = 不限）。 */
  maxStartsInWindow: number;
}

/** `[tasks.stop]` */
export interface TaskDraftStop {
  method: string;
  /** 优雅停止宽限（毫秒，100–60000）。 */
  graceMs: number;
  timeoutMs: number;
}

/** `[tasks.logs]` */
export interface TaskDraftLogs {
  /** 日志目录；`null` 表示用 `<data>/logs/<id>`。 */
  dir: string | null;
  rotation: string;
  maxFileMb: number;
  maxFiles: number;
  maxDays: number;
  encoding: string;
}

/** `[tasks.power]` */
export interface TaskDraftPower {
  /** `null` 表示继承 `[supervisor].resume_grace_ms`。 */
  resumeGraceMs: number | null;
}

/** 保存/删除草稿的结果（`save_task_draft` / `delete_task`）。 */
export interface TaskDraftResult {
  /** 落盘后文件内容的 revision。 */
  newRevision: string;
  /** false = 草稿与磁盘一致，未写盘、未触发重载。 */
  changed: boolean;
  /** 重新加载后的完整快照。 */
  snapshot: Snapshot;
}

/** 草稿命令失败时的错误 DTO（形状统一为 {@link IpcError}）。 */
export type TaskDraftError = IpcError;

/** 开机自启模式（`autostart.mode`）。 */
export type AutostartMode = "run_key" | "scheduled_task" | "disabled";

/**
 * 开机自启只读状态（M4-3，`get_autostart_status`）。
 *
 * 只描述现状：后端绝不写注册表；取 exe 或读取失败时只填 `error`。
 */
export interface AutostartStatus {
  /** 配置中的 `autostart.enabled`。 */
  enabled: boolean;
  mode: AutostartMode;
  /** 按配置与当前 exe 推导出的期望 Run 值；不需要注册时为 null。 */
  expectedValue: string | null;
  /** 注册表中实际读到的 Run 值；不存在或读取失败时为 null。 */
  actualValue: string | null;
  /** 注册表现状是否与配置期望一致；读取失败时为 false。 */
  matches: boolean;
  /** 注册表中是否存在该值；读取失败时为 false。 */
  registered: boolean;
  /** 取 exe 或读取注册表失败时的说明；正常为 null。 */
  error: string | null;
}

/**
 * 系统 Toast 状态（`get_toast_status` / `test_toast`）。
 *
 * 后端只读取用户提供的开始菜单快捷方式；不创建/修改快捷方式，也不写
 * HKCU AppUserModelId。`available=false` 时 `reason` 给出一行原因。
 */
export interface ToastStatus {
  available: boolean;
  /** 不可用原因；可用时为 null。 */
  reason: string | null;
  /** 作为 AUMID 的当前 exe 路径。 */
  aumid: string | null;
  /** 实际找到的开始菜单快捷方式路径；未找到时为 null。 */
  shortcut: string | null;
  /** 最近一次发送失败的错误；没有失败时为 null。 */
  lastError: string | null;
}

/** 插件来源视图（TaskSnapshot.plugin）。 */
export interface PluginView {
  id: string;
  /** 清单绝对路径。 */
  manifestPath: string;
  /** 导入时的版本；清单升级后由界面提示同步。 */
  version: string | null;
  /** 清单版本高于已存版本（配置加载时比对；只提示，不自动同步）。 */
  updateAvailable: boolean;
}

/** 一个运行时段。 */
export interface ScheduleWindowDraft {
  /** mon..sun。 */
  days: string[];
  /** HH:MM。 */
  start: string;
  /** HH:MM。 */
  end: string;
}

/**
 * 每周运行时段视图（TaskSnapshot.schedule）。
 *
 * 2026-11-05 架构修正：`[tasks.schedule]` 是任务唯一的时钟；`enabled=true` 时由
 * 应用唯一决定启停，脚本内部不再有自动开关/课表。进入并集窗口自动启动一次；
 * 窗口结束停止该任务任何存活实例（含手动启动、含 needs_human）。
 *
 * v2（2026-11-15）：v1 的三个兼容字段（两个时段开关 + 启动来源标记，见
 * docs/19 B4）已从 DTO 删除；旧快照里多出的键会被忽略。
 */
export interface ScheduleView {
  enabled: boolean;
  windows: ScheduleWindowDraft[];
  /** 当前是否处于某个时段（并集）内。 */
  inWindow: boolean;
  /** 下次进入时段的时刻（Unix epoch 毫秒）。 */
  nextStartAt: number | null;
  /** 当前时段（窗口内）或下次时段（窗口外）并集结束的时刻（Unix epoch 毫秒）。 */
  nextEndAt: number | null;
}

/**
 * 运行时段编辑草稿（get_schedule / save_schedule 载荷）。
 *
 * v2（2026-11-15）：已删除 v1 的两个废弃开关（见 docs/19 B4）；旧载荷里的
 * 多余同名键会被后端忽略，不影响保存。
 */
export interface ScheduleDraft {
  enabled: boolean;
  windows: ScheduleWindowDraft[];
}

/** 读取运行时段的结果（get_schedule）。 */
export interface ScheduleEditorView {
  taskId: string;
  schedule: ScheduleDraft;
  /** 整份配置文件内容的 revision（与文本编辑器同算法）。 */
  revision: string;
}

/** 保存运行时段的结果（save_schedule）。 */
export interface SaveScheduleResult {
  revision: string;
  changed: boolean;
  snapshot: Snapshot;
}

/** 运行时段命令失败 DTO（形状统一为 {@link IpcError}）。 */
export type ScheduleCommandError = IpcError;

/** 插件清单预览（inspect_plugin）。字段与后端 PluginPreview 一一对应。 */
export interface PluginPreview {
  manifestPath: string;
  rootDir: string;
  pluginId: string;
  name: string;
  version: string;
  description: string | null;
  homepage: string | null;
  command: string;
  args: string[];
  workingDir: string;
  ports: number[];
  env: Record<string, string>;
  envProxy: string;
  panelUrl: string | null;
  healthType: string;
  healthUrl: string | null;
  schedule: ScheduleDraft | null;
  /** 建议的任务 id（当前等于 pluginId）。 */
  taskId: string;
  /** 配置里已有同 id 任务时为该 id；导入会失败，引导 sync。 */
  existingTaskId: string | null;
  warnings: string[];
}

/** 插件预览 + 当前配置 revision。 */
export interface PluginInspectView extends PluginPreview {
  configRevision: string;
}

/** 导入插件的结果。 */
export interface PluginImportResult {
  taskId: string;
  snapshot: Snapshot;
}

/** 同步插件清单的结果。 */
export interface PluginSyncResult {
  taskId: string;
  changed: boolean;
  snapshot: Snapshot;
}

/** 插件命令失败 DTO（形状统一为 {@link IpcError}）。 */
export type PluginCommandError = IpcError;

/** 核心事件日志视图（read_event_log）。 */
export interface EventLogView {
  path: string;
  /** 原始 JSONL 行；UI 可按原文展示/搜索。 */
  lines: string[];
  truncated: boolean;
  totalBytes: number;
}

/** 诊断导出结果（export_diagnostics）。 */
export interface DiagnosticsResult {
  /** 导出目录绝对路径。 */
  path: string;
  files: number;
}

/** 用系统资源管理器打开的应用目录（白名单，前端不能传任意路径）。 */
export type AppPathKind = "logs" | "data" | "config_dir";

// ---------------------------------------------------------------------------
// 命令门面
//
// 这里只做一件事：把调用转发给当前传输层（Tauri IPC 或浏览器演示后端）。
// 业务编排（快照刷新、错误落位、批量操作、冲突处理）在 lib/services/*；
// 页面组件不直接 invoke 任何命令。
// ---------------------------------------------------------------------------

/** 读取当前快照（不重新加载配置文件）。 */
export async function getSnapshot(): Promise<Snapshot> {
  return (await getTransport()).getSnapshot();
}

/** 重新加载配置文件；后端会同时广播 snapshot:changed。 */
export async function reloadConfig(): Promise<Snapshot> {
  return (await getTransport()).reloadConfig();
}

/** 退出应用：后端停止全部任务、保存 state、刷新日志后退出。 */
export async function quitApp(): Promise<void> {
  return (await getTransport()).quitApp();
}

/** 确认已展示“仍在托盘运行”的一次性提示；返回更新后的快照。 */
export async function acknowledgeTrayHint(): Promise<Snapshot> {
  return (await getTransport()).acknowledgeTrayHint();
}

/** 只读查询开机自启状态（后端绝不写注册表）。 */
export async function getAutostartStatus(): Promise<AutostartStatus> {
  return (await getTransport()).getAutostartStatus();
}

/** 只读查询系统通知状态；不创建/修改开始菜单快捷方式。 */
export async function getToastStatus(): Promise<ToastStatus> {
  return (await getTransport()).getToastStatus();
}

/** 发送一条测试系统通知；返回发送后的状态。 */
export async function testToast(): Promise<ToastStatus> {
  return (await getTransport()).testToast();
}

/** 启动任务；失败时 reject 一个 {@link IpcError}。 */
export async function startTask(id: string): Promise<TaskCommandResult> {
  return (await getTransport()).startTask(id);
}

/** 停止任务（最多等 [tasks.stop].timeout_ms）。 */
export async function stopTask(id: string): Promise<TaskCommandResult> {
  return (await getTransport()).stopTask(id);
}

/** 停止后立即启动新实例。 */
export async function restartTask(id: string): Promise<TaskCommandResult> {
  return (await getTransport()).restartTask(id);
}

/** 读取任务日志尾部。只读：目录与文件名由后端白名单解析。 */
export async function readTaskLog(
  taskId: string,
  query: TaskLogQuery = {},
): Promise<TaskLogView> {
  return (await getTransport()).readTaskLog(taskId, query);
}

/** 列出任务的日志文件：current.log 在前，归档按 mtime 倒序。 */
export async function listTaskLogs(taskId: string): Promise<TaskLogFilesView> {
  return (await getTransport()).listTaskLogs(taskId);
}

/** 读取当前配置文件的文本与 revision。 */
export async function readConfigText(): Promise<ConfigTextView> {
  return (await getTransport()).readConfigText();
}

/** 保存配置文本；revision 冲突/结构错误都不会写盘。 */
export async function saveConfigText(
  text: string,
  expectedRevision: string,
): Promise<SaveConfigTextResult> {
  return (await getTransport()).saveConfigText(text, expectedRevision);
}

/** 用配置文件旁的 .bak 覆盖当前配置；坏文件另存为 .bak.bad。 */
export async function restoreConfigBackup(): Promise<SaveConfigTextResult> {
  return (await getTransport()).restoreConfigBackup();
}

/** 新建任务表单的默认值（来自 Rust 侧 TaskDraft::default()）。 */
export async function getTaskDefaults(): Promise<TaskDraft> {
  return (await getTransport()).getTaskDefaults();
}

/** 读取某个任务的结构化草稿（只读）。 */
export async function getTaskDraft(taskId: string): Promise<TaskDraft> {
  return (await getTransport()).getTaskDraft(taskId);
}

/** 保存结构化任务草稿；只改草稿涉及的值，保留注释与其他任务。 */
export async function saveTaskDraft(
  draft: TaskDraft,
  expectedRevision: string,
): Promise<TaskDraftResult> {
  return (await getTransport()).saveTaskDraft(draft, expectedRevision);
}

/** 删除某个任务；冲突检测与 saveTaskDraft 相同。 */
export async function deleteTask(
  taskId: string,
  expectedRevision: string,
): Promise<TaskDraftResult> {
  return (await getTransport()).deleteTask(taskId, expectedRevision);
}

/** 按给定顺序重写任务展示顺序；ids 必须与现有任务集合完全一致。 */
export async function reorderTasks(
  ids: string[],
  expectedRevision: string,
): Promise<TaskDraftResult> {
  return (await getTransport()).reorderTasks(ids, expectedRevision);
}

/** 读取任务的每周运行时段草稿；revision 是整份配置文件的 revision。 */
export async function getSchedule(taskId: string): Promise<ScheduleEditorView> {
  return (await getTransport()).getSchedule(taskId);
}

/** 保存每周运行时段：只改该任务的 [tasks.schedule]，保留注释与其他任务。 */
export async function saveSchedule(
  taskId: string,
  draft: ScheduleDraft,
  expectedRevision: string,
): Promise<SaveScheduleResult> {
  return (await getTransport()).saveSchedule(taskId, draft, expectedRevision);
}

/** 读取插件清单并预览；不写配置。 */
export async function inspectPlugin(manifestPath: string): Promise<PluginInspectView> {
  return (await getTransport()).inspectPlugin(manifestPath);
}

/** 把插件清单导入为普通 [[tasks]]；任务 id 冲突时 reject。 */
export async function importPlugin(
  manifestPath: string,
  expectedRevision: string,
): Promise<PluginImportResult> {
  return (await getTransport()).importPlugin(manifestPath, expectedRevision);
}

/** 用清单更新已有插件任务的“清单拥有字段”，保留用户偏好字段。 */
export async function syncPlugin(
  taskId: string,
  expectedRevision: string,
): Promise<PluginSyncResult> {
  return (await getTransport()).syncPlugin(taskId, expectedRevision);
}

/** 用系统默认浏览器打开任务的 panel_url（只接受 http/https）。 */
export async function openTaskPanel(taskId: string): Promise<void> {
  return (await getTransport()).openTaskPanel(taskId);
}

/** 用资源管理器打开应用目录（后端只接受白名单 kind）。 */
export async function openAppPath(kind: AppPathKind): Promise<void> {
  return (await getTransport()).openAppPath(kind);
}

/** 读取核心事件日志（JSONL）尾部。 */
export async function readEventLog(
  tailLines?: number,
  maxBytes?: number,
): Promise<EventLogView> {
  return (await getTransport()).readEventLog(tailLines, maxBytes);
}

/** 导出诊断包到数据目录 diagnostics/<时间戳>/。 */
export async function exportDiagnostics(): Promise<DiagnosticsResult> {
  return (await getTransport()).exportDiagnostics();
}

/** 订阅快照变化；返回取消订阅函数。 */
export async function onSnapshotChanged(
  handler: (snapshot: Snapshot) => void,
): Promise<UnlistenFn> {
  return (await getTransport()).onSnapshotChanged(handler);
}

/** 订阅需要人介入的通知；返回取消订阅函数。 */
export async function onNotificationRaised(
  handler: (notification: NotificationPayload) => void,
): Promise<UnlistenFn> {
  return (await getTransport()).onNotificationRaised(handler);
}

export type { UnlistenFn };
