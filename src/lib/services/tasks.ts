// 任务动作编排：启停/重启、批量、排序、插件同步、打开控制台。
//
// 组件只调用这里，不直接 invoke 命令；这里统一负责“命令返回后用快照覆盖 UI”、
// 错误落位（app.taskErrors）与通知已读收敛。

import {
  openTaskPanel,
  readConfigText,
  reorderTasks,
  restartTask,
  startTask,
  stopTask,
  syncPlugin,
  type Snapshot,
  type TaskSnapshot,
} from "$lib/api";
import { app } from "$lib/state/app.svelte";
import { errorMessage } from "$lib/utils/task-utils";
import type { TaskAction } from "$lib/utils/task-utils";

/** 批量启停时同时进行的命令数上限。 */
export const BATCH_CONCURRENCY = 3;

export type BatchReport = { ok: number; failed: number; message: string };

/**
 * 单任务动作：启动 / 停止 / 重启。
 *
 * 失败只写到该任务的错误位，不打断页面；成功后把该任务的通知标为已读——
 * 用户已经动手处理过，不需要再提示一次。
 */
export async function runTaskAction(task: TaskSnapshot, action: TaskAction): Promise<void> {
  if (app.isTaskBusy(task.id)) {
    return;
  }
  app.taskBusy = { ...app.taskBusy, [task.id]: action };
  app.taskErrors = { ...app.taskErrors, [task.id]: null };
  try {
    const result = await (
      action === "start" ? startTask : action === "stop" ? stopTask : restartTask
    )(task.id);
    app.applySnapshot(result.snapshot);
    app.notifications = app.notifications.map((item) =>
      item.taskId === task.id && !item.read ? { ...item, read: true } : item,
    );
    app.toasts = app.toasts.filter((key) => {
      const item = app.notifications.find((entry) => entry.key === key);
      return item !== undefined && item.taskId !== task.id;
    });
  } catch (error) {
    app.taskErrors = { ...app.taskErrors, [task.id]: errorMessage(error) };
  } finally {
    app.taskBusy = { ...app.taskBusy, [task.id]: null };
  }
}

/** 批量启停：按快照顺序执行，并发上限 BATCH_CONCURRENCY，单项失败不影响其余。 */
export async function runBatch(kind: "start" | "stop"): Promise<BatchReport> {
  const verb = kind === "start" ? "启动" : "停止";
  const targets = app.tasks.filter((task) => task.allowedActions?.[kind] === true);
  if (targets.length === 0) {
    return { ok: 0, failed: 0, message: "没有需要" + verb + "的脚本" };
  }
  let ok = 0;
  const failures: string[] = [];
  await forEachLimit(targets, BATCH_CONCURRENCY, async (task) => {
    try {
      const result = await (kind === "start" ? startTask : stopTask)(task.id);
      app.applySnapshot(result.snapshot);
      ok += 1;
    } catch (error) {
      failures.push((task.name || task.id) + "：" + errorMessage(error));
    }
  });
  return {
    ok,
    failed: failures.length,
    message:
      failures.length === 0
        ? "已" + verb + " " + ok + " 个脚本"
        : "成功 " + ok + "，失败 " + failures.length + "（" + failures[0] + "）",
  };
}

/** 简易并发池：同时最多 limit 个 worker 消费同一队列。 */
async function forEachLimit<T>(
  items: T[],
  limit: number,
  worker: (item: T) => Promise<void>,
): Promise<void> {
  let next = 0;
  const runners = Array.from({ length: Math.min(limit, items.length) }, async () => {
    while (next < items.length) {
      const item = items[next];
      next += 1;
      await worker(item);
    }
  });
  await Promise.all(runners);
}

/** 上/下移持久化：把完整 id 顺序交给后端重写 order。 */
export async function moveTask(from: number, to: number): Promise<void> {
  const ids = app.tasks.map((task) => task.id);
  if (to < 0 || to >= ids.length) {
    return;
  }
  const [moved] = ids.splice(from, 1);
  ids.splice(to, 0, moved);
  const revision = app.snapshot?.configRevision ?? "";
  const result = await reorderTasks(ids, revision);
  app.applySnapshot(result.snapshot);
}

/** 从清单同步插件任务；返回一句可展示的结果。 */
export async function syncTaskFromManifest(taskId: string): Promise<string> {
  const config = await readConfigText();
  const result = await syncPlugin(taskId, config.revision);
  app.applySnapshot(result.snapshot);
  return result.changed ? "已从清单同步" : "清单与配置一致";
}

/** 用系统浏览器打开脚本面板。 */
export async function openPanel(taskId: string): Promise<void> {
  await openTaskPanel(taskId);
}

/** 把快照写回 store（服务层内部与页面都需要时用）。 */
export function applySnapshot(snapshot: Snapshot): void {
  app.applySnapshot(snapshot);
}
