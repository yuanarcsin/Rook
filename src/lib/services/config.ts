// 配置相关编排：revision 读取、重新加载、备份恢复。
//
// 三个入口共享同一条规则：先用快照里的 configRevision（B6），结构错误等
// 不可用场景才回退到读取整份 TOML。

import {
  readConfigText,
  reloadConfig,
  restoreConfigBackup,
  type SaveConfigTextResult,
} from "$lib/api";
import { app } from "$lib/state/app.svelte";

/** 当前磁盘 revision；优先快照，其次读文本。 */
export async function currentConfigRevision(): Promise<string> {
  const fromSnapshot = app.snapshot?.configRevision ?? "";
  return fromSnapshot.length > 0 ? fromSnapshot : (await readConfigText()).revision;
}

/** 重新读取磁盘配置（工具栏/设置页共用）。 */
export async function reloadFromDisk(): Promise<void> {
  if (app.busy) {
    return;
  }
  app.busy = true;
  try {
    app.applySnapshot(await reloadConfig());
  } finally {
    app.busy = false;
  }
}

/** 用 .bak 覆盖当前配置；成功返回新快照。 */
export async function restoreBackup(): Promise<SaveConfigTextResult> {
  const result = await restoreConfigBackup();
  app.applySnapshot(result.snapshot);
  return result;
}

/** 备份文件名（用于确认文案）：<配置文件名>.bak。 */
export function backupFileName(): string {
  const path = app.snapshot?.configPath ?? "";
  const name = path.slice(Math.max(path.lastIndexOf("/"), path.lastIndexOf(String.fromCharCode(92))) + 1);
  return name ? name + ".bak" : ".bak";
}
