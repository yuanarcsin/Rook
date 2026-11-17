// 传输层选择器：前端只说“我要什么数据”，不关心数据来自 Tauri IPC 还是浏览器里的演示数据。
//
// - 在 Tauri 运行时（`__TAURI_INTERNALS__` 存在）动态加载 `./tauri`；
// - 其余情况（`pnpm dev` 直接开浏览器、或静态产物被浏览器打开）动态加载 `./mock`。
//
// 这样 UI 可以脱离 Rust 独立开发与验收（前后端分离），生产包也不会把演示数据
// 塞进首屏 chunk：mock 是按需加载的独立 chunk。

import type {
  AutostartStatus,
  ConfigTextView,
  DiagnosticsResult,
  EventLogView,
  NotificationPayload,
  PluginImportResult,
  PluginInspectView,
  PluginSyncResult,
  SaveConfigTextResult,
  SaveScheduleResult,
  ScheduleDraft,
  ScheduleEditorView,
  Snapshot,
  TaskCommandResult,
  TaskDraft,
  TaskDraftResult,
  TaskLogFilesView,
  TaskLogQuery,
  TaskLogView,
  ToastStatus,
  UnlistenFn,
} from "$lib/api";

/** 全部后端能力的最小接口；`api.ts` 只做转发，组件不直接碰 invoke。 */
export interface Backend {
  getSnapshot(): Promise<Snapshot>;
  reloadConfig(): Promise<Snapshot>;
  quitApp(): Promise<void>;
  acknowledgeTrayHint(): Promise<Snapshot>;
  getAutostartStatus(): Promise<AutostartStatus>;
  getToastStatus(): Promise<ToastStatus>;
  testToast(): Promise<ToastStatus>;

  startTask(id: string): Promise<TaskCommandResult>;
  stopTask(id: string): Promise<TaskCommandResult>;
  restartTask(id: string): Promise<TaskCommandResult>;

  readTaskLog(taskId: string, query?: TaskLogQuery): Promise<TaskLogView>;
  listTaskLogs(taskId: string): Promise<TaskLogFilesView>;

  readConfigText(): Promise<ConfigTextView>;
  saveConfigText(text: string, expectedRevision: string): Promise<SaveConfigTextResult>;
  restoreConfigBackup(): Promise<SaveConfigTextResult>;

  getTaskDefaults(): Promise<TaskDraft>;
  getTaskDraft(taskId: string): Promise<TaskDraft>;
  saveTaskDraft(draft: TaskDraft, expectedRevision: string): Promise<TaskDraftResult>;
  deleteTask(taskId: string, expectedRevision: string): Promise<TaskDraftResult>;
  reorderTasks(ids: string[], expectedRevision: string): Promise<TaskDraftResult>;

  getSchedule(taskId: string): Promise<ScheduleEditorView>;
  saveSchedule(taskId: string, draft: ScheduleDraft, expectedRevision: string): Promise<SaveScheduleResult>;

  inspectPlugin(manifestPath: string): Promise<PluginInspectView>;
  importPlugin(manifestPath: string, expectedRevision: string): Promise<PluginImportResult>;
  syncPlugin(taskId: string, expectedRevision: string): Promise<PluginSyncResult>;

  openTaskPanel(taskId: string): Promise<void>;
  openAppPath(kind: "logs" | "data" | "config_dir"): Promise<void>;
  readEventLog(tailLines?: number, maxBytes?: number): Promise<EventLogView>;
  exportDiagnostics(): Promise<DiagnosticsResult>;

  onSnapshotChanged(handler: (snapshot: Snapshot) => void): Promise<UnlistenFn>;
  onNotificationRaised(handler: (notification: NotificationPayload) => void): Promise<UnlistenFn>;
}

let transport: Promise<Backend> | null = null;

/** 当前是否是浏览器演示模式（不含 Tauri 宿主）。 */
export function isMockMode(): boolean {
  return typeof window !== "undefined" && !("__TAURI_INTERNALS__" in window);
}

/** 取传输层单例；首次调用时按运行环境决定加载哪一个 chunk。 */
export function getTransport(): Promise<Backend> {
  transport ??= createTransport();
  return transport;
}

async function createTransport(): Promise<Backend> {
  if (isMockMode()) {
    const { mockBackend } = await import("./mock");
    return mockBackend;
  }
  const { tauriBackend } = await import("./tauri");
  return tauriBackend;
}
