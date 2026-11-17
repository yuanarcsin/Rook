// Tauri 传输层：唯一直接使用 @tauri-apps/api 的地方。
//
// 命令名/参数形状与 `src-tauri/src/lib.rs` 的 `generate_handler!` 一一对应；
// 返回值类型来自 `$lib/api`（由 Rust DTO serde camelCase 输出）。

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
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
} from "$lib/api";
import type { Backend } from "./index";

export const tauriBackend: Backend = {
  getSnapshot: () => invoke<Snapshot>("get_snapshot"),
  reloadConfig: () => invoke<Snapshot>("reload_config"),
  quitApp: () => invoke<void>("quit_app"),
  acknowledgeTrayHint: () => invoke<Snapshot>("acknowledge_tray_hint"),
  getAutostartStatus: () => invoke<AutostartStatus>("get_autostart_status"),
  getToastStatus: () => invoke<ToastStatus>("get_toast_status"),
  testToast: () => invoke<ToastStatus>("test_toast"),

  startTask: (id) => invoke<TaskCommandResult>("start_task", { id }),
  stopTask: (id) => invoke<TaskCommandResult>("stop_task", { id }),
  restartTask: (id) => invoke<TaskCommandResult>("restart_task", { id }),

  readTaskLog: (taskId, query: TaskLogQuery = {}) =>
    invoke<TaskLogView>("read_task_log", {
      taskId,
      fileId: query.fileId ?? null,
      tailLines: query.tailLines ?? null,
      maxBytes: query.maxBytes ?? null,
    }),
  listTaskLogs: (taskId) => invoke<TaskLogFilesView>("list_task_logs", { taskId }),

  readConfigText: () => invoke<ConfigTextView>("read_config_text"),
  saveConfigText: (text, expectedRevision) =>
    invoke<SaveConfigTextResult>("save_config_text", { text, expectedRevision }),
  restoreConfigBackup: () => invoke<SaveConfigTextResult>("restore_config_backup"),

  getTaskDefaults: () => invoke<TaskDraft>("get_task_defaults"),
  getTaskDraft: (taskId) => invoke<TaskDraft>("get_task_draft", { taskId }),
  saveTaskDraft: (draft, expectedRevision) =>
    invoke<TaskDraftResult>("save_task_draft", { draft, expectedRevision }),
  deleteTask: (taskId, expectedRevision) =>
    invoke<TaskDraftResult>("delete_task", { taskId, expectedRevision }),
  reorderTasks: (ids, expectedRevision) =>
    invoke<TaskDraftResult>("reorder_tasks", { ids, expectedRevision }),

  getSchedule: (taskId) => invoke<ScheduleEditorView>("get_schedule", { taskId }),
  saveSchedule: (taskId, draft: ScheduleDraft, expectedRevision) =>
    invoke<SaveScheduleResult>("save_schedule", { taskId, draft, expectedRevision }),

  inspectPlugin: (manifestPath) =>
    invoke<PluginInspectView>("inspect_plugin", { manifestPath }),
  importPlugin: (manifestPath, expectedRevision) =>
    invoke<PluginImportResult>("import_plugin", { manifestPath, expectedRevision }),
  syncPlugin: (taskId, expectedRevision) =>
    invoke<PluginSyncResult>("sync_plugin", { taskId, expectedRevision }),

  openTaskPanel: (taskId) => invoke<void>("open_task_panel", { taskId }),
  openAppPath: (kind) => invoke<void>("open_app_path", { kind }),
  readEventLog: (tailLines?: number, maxBytes?: number) =>
    invoke<EventLogView>("read_event_log", {
      tailLines: tailLines ?? null,
      maxBytes: maxBytes ?? null,
    }),
  exportDiagnostics: () => invoke<DiagnosticsResult>("export_diagnostics"),

  onSnapshotChanged: (handler) =>
    listen<Snapshot>("snapshot:changed", (event) => handler(event.payload)),
  onNotificationRaised: (handler) =>
    listen<NotificationPayload>("notification:raised", (event) => handler(event.payload)),
};

export type { UnlistenFn };
