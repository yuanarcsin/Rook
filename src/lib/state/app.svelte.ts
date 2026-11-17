// 全局 UI 状态（Svelte 5 runes）：快照、导航、编辑器入口、通知。
//
// 只放“跨页面共享”的状态。页面内部的筛选、草稿、轮询节奏留在各自组件或
// 对应的 controller（state/task-editor、state/log-view），关闭即释放。

import {
  getSnapshot,
  onNotificationRaised,
  onSnapshotChanged,
  reloadConfig,
  type NotificationPayload,
  type Snapshot,
  type TaskSnapshot,
  type UnlistenFn,
} from "$lib/api";
import {
  errorMessage,
  needsAttention,
  type TaskAction,
} from "$lib/utils/task-utils";

export type View = "console" | "logs" | "settings" | "task";

/** 外观主题：跟随系统 / 固定浅色 / 固定深色。只影响本机界面，不写入配置。 */
export type ThemePref = "system" | "light" | "dark";

export type EditorMode = "new" | "edit";

export type EditorState = {
  open: boolean;
  mode: EditorMode;
  /** 编辑时的任务 id；新建为空串。 */
  taskId: string;
  /** 每次打开都自增，用于让编辑器重建并重新读取草稿。 */
  session: number;
};

/** 页内通知条目：后端负载 + 本地状态（已读/到达时间/自增 key）。 */
export type NotificationItem = NotificationPayload & {
  key: number;
  receivedAt: number;
  read: boolean;
};

const MAX_NOTIFICATIONS = 50;
const MAX_TOASTS = 3;
/** 配置提醒是快照派生的单条通知，用固定负 key 与后端事件通知区分。 */
const CONFIG_WARNING_KEY = -1;
/** 侧栏收起偏好；localStorage 不可用时只保留本次会话状态。 */
const SIDEBAR_KEY = "rook.sidebar.collapsed";
/** 主题偏好；app.html 的首屏内联脚本读取同一个键。 */
const THEME_KEY = "rook.theme";

let notificationSeq = 0;

class AppStore {
  snapshot = $state<Snapshot | null>(null);
  loadError = $state<string | null>(null);
  /** 首次加载中（骨架屏）；重新加载用 busy。 */
  loading = $state(true);
  busy = $state(false);
  view = $state<View>("console");
  /** 侧栏是否收起为图标轨（桌面壳层偏好，持久化到 localStorage）。 */
  sidebarCollapsed = $state(false);
  /** 外观主题偏好；实际生效值写进 <html data-theme>。 */
  themePref = $state<ThemePref>("system");
  /** 用户显式选择的侧栏偏好；窄窗自动收起不写它。 */
  private sidebarPreference = false;
  /** 当前收起是否由窄窗自动触发；放宽窗口时据此恢复偏好。 */
  private sidebarAuto = false;
  /** 系统深浅色查询；pref=system 时监听变化。 */
  private systemTheme: MediaQueryList | null = null;
  /** 脚本页当前脚本；仅 view === "task" 时有效。 */
  activeTaskId = $state<string | null>(null);
  editor = $state<EditorState>({ open: false, mode: "new", taskId: "", session: 0 });
  /** 插件导入弹窗。 */
  importOpen = $state(false);
  /** TOML 配置编辑器弹窗。 */
  configOpen = $state(false);
  /** 日志页希望默认选中的任务（从通知/详情跳转时写入）。 */
  logTaskId = $state<string | null>(null);
  /** 日志页当前分段：任务日志 / 应用事件。 */
  logSegment = $state<"task" | "event">("task");
  /** 每个任务当前进行中的动作；空表示空闲。 */
  taskBusy = $state<Record<string, TaskAction | null>>({});
  /** 每个任务最近一次操作失败的信息。 */
  taskErrors = $state<Record<string, string | null>>({});
  notifications = $state<NotificationItem[]>([]);
  notificationsOpen = $state(false);
  /** 面板关闭时新通知的轻提示，存 NotificationItem.key。 */
  toasts = $state<number[]>([]);
  /** 已同步过的配置 warning 指纹：相同就不重复提醒，也不在手动删除后反复出现。 */
  private configWarningSignature = "";

  get tasks(): TaskSnapshot[] {
    return this.snapshot?.tasks ?? [];
  }

  get attentionTasks(): TaskSnapshot[] {
    return this.tasks.filter(needsAttention);
  }

  get runningCount(): number {
    return this.tasks.filter((task) => task.displayStatus === "running").length;
  }

  /** 脚本页展示的脚本；被删除/切走时自动失效。 */
  get activeTask(): TaskSnapshot | null {
    if (this.activeTaskId === null) {
      return null;
    }
    return this.tasks.find((task) => task.id === this.activeTaskId) ?? null;
  }

  get unreadCount(): number {
    return this.notifications.reduce((count, item) => (item.read ? count : count + 1), 0);
  }

  /** 轻提示队列对应的通知条目（按到达顺序，最新在最后）。 */
  get toastItems(): NotificationItem[] {
    const byKey = new Map(this.notifications.map((item) => [item.key, item]));
    return this.toasts
      .map((key) => byKey.get(key))
      .filter((item): item is NotificationItem => item !== undefined);
  }

  setView(view: View) {
    this.view = view;
    if (view !== "task") {
      this.activeTaskId = null;
    }
  }

  /** 恢复壳层偏好（侧栏收起、外观主题）；在应用挂载时调用一次。 */
  initChrome() {
    try {
      this.sidebarPreference = window.localStorage.getItem(SIDEBAR_KEY) === "1";
      const savedTheme = window.localStorage.getItem(THEME_KEY);
      if (savedTheme === "system" || savedTheme === "light" || savedTheme === "dark") {
        this.themePref = savedTheme;
      }
    } catch {
      // localStorage 被禁用时按默认值处理。
    }
    this.sidebarCollapsed = this.sidebarPreference;
    this.applyTheme();
    if (this.systemTheme === null) {
      this.systemTheme = window.matchMedia("(prefers-color-scheme: dark)");
      this.systemTheme.addEventListener("change", () => {
        if (this.themePref === "system") {
          this.applyTheme();
        }
      });
    }
  }

  /** 切换外观主题：写入 <html data-theme>，并持久化到本机。 */
  setThemePref(pref: ThemePref) {
    this.themePref = pref;
    this.applyTheme();
    try {
      window.localStorage.setItem(THEME_KEY, pref);
    } catch {
      // 偏好只在本次会话生效。
    }
  }

  private applyTheme() {
    if (typeof document === "undefined") {
      return;
    }
    const dark =
      this.themePref === "dark" ||
      (this.themePref === "system" &&
        window.matchMedia("(prefers-color-scheme: dark)").matches);
    document.documentElement.dataset.theme = dark ? "dark" : "light";
  }

  /**
   * 窄窗自动收起侧栏（不写偏好）；窗口放宽后回到用户手动选择的状态。
   * 用户在窄窗里手动展开/收起会转成显式偏好，不再被自动逻辑覆盖。
   */
  syncSidebarForWidth(narrow: boolean) {
    if (narrow) {
      if (!this.sidebarCollapsed) {
        this.sidebarAuto = true;
      }
      this.sidebarCollapsed = true;
    } else if (this.sidebarAuto) {
      this.sidebarAuto = false;
      this.sidebarCollapsed = this.sidebarPreference;
    }
  }

  setSidebarCollapsed(collapsed: boolean) {
    this.sidebarAuto = false;
    this.sidebarPreference = collapsed;
    this.sidebarCollapsed = collapsed;
    try {
      window.localStorage.setItem(SIDEBAR_KEY, collapsed ? "1" : "0");
    } catch {
      // 偏好只在本次会话生效。
    }
  }

  toggleSidebar() {
    this.setSidebarCollapsed(!this.sidebarCollapsed);
  }

  /** 从脚本页/通知跳到日志页并选中该脚本。 */
  openLogsFor(taskId: string) {
    this.logTaskId = taskId;
    this.setView("logs");
  }

  /** 打开某个脚本的独立页面（侧栏与卡片共用）。 */
  openTask(taskId: string) {
    this.activeTaskId = taskId;
    this.view = "task";
  }

  applySnapshot(next: Snapshot) {
    this.snapshot = next;
    this.loadError = null;
    if (this.activeTaskId !== null && !next.tasks.some((task) => task.id === this.activeTaskId)) {
      this.activeTaskId = null;
      if (this.view === "task") {
        this.view = "console";
      }
    }
    this.syncConfigWarnings();
  }

  /**
   * 配置 warning 不再占页面横幅：合并成通知中心里的一条“配置提醒”。
   * - 没有 warning 时自动移除；
   * - 相同指纹不重复；手动删掉后，除非 warning 变化才会再出现；
   * - 不生成轻提示，避免打断。
   */
  syncConfigWarnings() {
    const warnings = this.snapshot?.issues.filter((issue) => issue.severity === "warning") ?? [];
    const existing = this.notifications.find((item) => item.key === CONFIG_WARNING_KEY);
    if (warnings.length === 0) {
      this.configWarningSignature = "";
      if (existing) {
        this.notifications = this.notifications.filter((item) => item.key !== CONFIG_WARNING_KEY);
      }
      return;
    }
    const names = new Map(this.tasks.map((task) => [task.id, task.name || task.id]));
    const signature = warnings
      .map((issue) => [issue.taskId ?? "", issue.path, issue.message].join("|"))
      .join("\n");
    if (signature === this.configWarningSignature) {
      return;
    }
    this.configWarningSignature = signature;
    const message = warnings
      .map((issue) => {
        const owner = issue.taskId ? names.get(issue.taskId) ?? issue.taskId : null;
        const where = issue.path ? `（${issue.path}）` : "";
        return issue.message + where + (owner ? " · " + owner : "");
      })
      .join("\n");
    const item: NotificationItem = {
      key: CONFIG_WARNING_KEY,
      kind: "config_warning",
      level: "medium",
      title: "配置提醒",
      message,
      taskId: null,
      taskName: null,
      count: warnings.length,
      at: Date.now(),
      receivedAt: Date.now(),
      read: false,
    };
    this.notifications = existing
      ? this.notifications.map((entry) => (entry.key === CONFIG_WARNING_KEY ? item : entry))
      : [item, ...this.notifications].slice(0, MAX_NOTIFICATIONS);
  }

  openNewTask() {
    this.editor = {
      open: true,
      mode: "new",
      taskId: "",
      session: this.editor.session + 1,
    };
  }

  openEditTask(taskId: string) {
    this.editor = {
      open: true,
      mode: "edit",
      taskId,
      session: this.editor.session + 1,
    };
  }

  closeEditor() {
    this.editor = { ...this.editor, open: false };
  }

  openImport() {
    this.importOpen = true;
  }

  closeImport() {
    this.importOpen = false;
  }

  openConfig() {
    this.configOpen = true;
  }

  closeConfig() {
    this.configOpen = false;
  }

  isTaskBusy(taskId: string): boolean {
    return this.taskBusy[taskId] != null;
  }

  /** 收到一条通知：入历史；面板关着时再补一条轻提示。 */
  pushNotification(payload: NotificationPayload) {
    const item: NotificationItem = {
      ...payload,
      key: (notificationSeq += 1),
      receivedAt: Date.now(),
      read: false,
    };
    this.notifications = [item, ...this.notifications].slice(0, MAX_NOTIFICATIONS);
    if (!this.notificationsOpen) {
      this.toasts = [...this.toasts, item.key].slice(-MAX_TOASTS);
    }
  }

  openNotifications() {
    this.notificationsOpen = true;
    this.toasts = [];
  }

  /** 关闭面板即视为“已看过”：未读角标随之清零。 */
  closeNotifications() {
    this.notificationsOpen = false;
    this.markAllRead();
  }

  toggleNotifications() {
    if (this.notificationsOpen) {
      this.closeNotifications();
    } else {
      this.openNotifications();
    }
  }

  markRead(key: number) {
    this.notifications = this.notifications.map((item) =>
      item.key === key && !item.read ? { ...item, read: true } : item,
    );
  }

  markAllRead() {
    this.notifications = this.notifications.map((item) =>
      item.read ? item : { ...item, read: true },
    );
  }

  dismissToast(key: number) {
    this.toasts = this.toasts.filter((item) => item !== key);
  }

  dismissNotification(key: number) {
    this.notifications = this.notifications.filter((item) => item.key !== key);
    this.toasts = this.toasts.filter((item) => item !== key);
  }

  clearNotifications() {
    this.notifications = [];
    this.toasts = [];
  }

  /** 首次加载：读快照；失败时保留错误给页面渲染重试入口。 */
  async load() {
    this.loading = true;
    try {
      this.applySnapshot(await getSnapshot());
    } catch (error) {
      this.loadError = errorMessage(error);
    } finally {
      this.loading = false;
    }
  }

  /** 重新加载磁盘配置；不清空当前快照，失败只提示。 */
  async reload() {
    if (this.busy) {
      return;
    }
    this.busy = true;
    try {
      this.applySnapshot(await reloadConfig());
    } catch (error) {
      this.loadError = errorMessage(error);
    } finally {
      this.busy = false;
    }
  }

  /**
   * 挂载时读取快照并订阅事件；返回清理函数。
   * 订阅失败不阻塞页面使用，只把原因写进 loadError。
   */
  async init(): Promise<() => void> {
    let unlisten: UnlistenFn | undefined;
    let unlistenNotifications: UnlistenFn | undefined;
    let cancelled = false;

    await this.load();
    if (cancelled) {
      return () => {};
    }
    try {
      unlisten = await onSnapshotChanged((next) => {
        this.applySnapshot(next);
      });
    } catch (error) {
      this.loadError = "订阅配置变化失败：" + errorMessage(error);
    }
    try {
      unlistenNotifications = await onNotificationRaised((notification) => {
        this.pushNotification(notification);
      });
    } catch (error) {
      this.loadError = "订阅通知失败：" + errorMessage(error);
    }

    return () => {
      cancelled = true;
      unlisten?.();
      unlistenNotifications?.();
    };
  }
}

/** 全局单例；组件只读状态、调用方法，不直接改字段。 */
export const app = new AppStore();

