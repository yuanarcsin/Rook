// 日志页控制器：任务/文件选择、读取、搜索过滤、跟随与轮询判定。
//
// 定时器与滚动副作用留在 LogsPage（它们需要组件生命周期）；这里只保留
// 可复用的读取/选择逻辑，避免页面文件里掺业务分支。

import { listTaskLogs, readTaskLog, type LogFileView, type TaskLogView } from "$lib/api";
import {
  filterLogLines,
  isStaleResponse,
  nextRequestSeq,
  shouldClearLoading,
} from "$lib/utils/log-poll";
import { countMatches, errorMessage } from "$lib/utils/task-utils";

export const CURRENT_LOG = "current.log";

export class LogViewState {
  taskId = $state<string | null>(null);
  fileId = $state<string | null>(null);
  tailLines = $state(200);
  autoRefresh = $state(true);
  follow = $state(true);
  search = $state("");
  files = $state<LogFileView[]>([]);
  filesError = $state<string | null>(null);
  view = $state<TaskLogView | null>(null);
  loading = $state(false);
  error = $state<string | null>(null);

  private requestSeq = 0;

  /** 当前选择解析成实际文件名：null 表示后端缺省的 current.log。 */
  get selectedFile(): string {
    return this.fileId ?? CURRENT_LOG;
  }

  /** 归档是静态文件，不参与自动轮询。 */
  get isArchive(): boolean {
    return this.selectedFile !== CURRENT_LOG;
  }

  get lines(): string[] {
    return this.view?.lines ?? [];
  }

  get filteredLines(): string[] {
    return filterLogLines(this.lines, this.search);
  }

  get matchCount(): number {
    return countMatches(this.lines, this.search);
  }

  get trimmedSearch(): string {
    return this.search.trim();
  }

  /** 切换任务：重置文件选择并重新列出日志文件（失败不阻塞日志读取）。 */
  async loadFiles(id: string | null): Promise<void> {
    if (id === null) {
      this.files = [];
      this.filesError = null;
      return;
    }
    try {
      const list = await listTaskLogs(id);
      if (id !== this.taskId) {
        return;
      }
      this.files = list.files;
      this.filesError = null;
      if (this.fileId !== null && !list.files.some((file) => file.id === this.fileId)) {
        this.fileId = null;
      }
    } catch (failure) {
      if (id !== this.taskId) {
        return;
      }
      this.files = [];
      this.filesError = errorMessage(failure);
    }
  }

  /** 读取一次日志；silent = 自动刷新，不显示“读取中”。 */
  async load(silent: boolean): Promise<void> {
    const id = this.taskId;
    if (id === null) {
      this.view = null;
      return;
    }
    const limit = this.tailLines;
    const file = this.fileId;
    const seq = (this.requestSeq = nextRequestSeq(this.requestSeq));
    if (!silent) {
      this.loading = true;
    }
    try {
      const next = await readTaskLog(id, { tailLines: limit, fileId: file });
      if (isStaleResponse(seq, this.requestSeq)) {
        return;
      }
      this.view = next;
      this.error = null;
    } catch (failure) {
      if (isStaleResponse(seq, this.requestSeq)) {
        return;
      }
      this.error = errorMessage(failure);
      if (!silent) {
        this.view = null;
      }
    } finally {
      if (shouldClearLoading(seq, this.requestSeq, silent)) {
        this.loading = false;
      }
    }
  }

  selectFile(next: string): void {
    // 只在文件真的变化时重置跟随；任务首屏（null -> 首个文件）不算切换。
    if (this.fileId !== null && this.fileId !== next) {
      this.follow = true;
    }
    this.fileId = next === CURRENT_LOG ? null : next;
  }

  async refresh(): Promise<void> {
    if (this.taskId === null) {
      return;
    }
    await Promise.all([this.load(false), this.loadFiles(this.taskId)]);
  }
}
