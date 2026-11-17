// 日志/事件读取的门面（组件不直接 import invoke）。
//
// 任务日志的“状态机”在 state/log-view；这里只给不持有状态的调用方用。

import { readEventLog, type EventLogView } from "$lib/api";

/** 读取应用事件日志（JSONL）尾部。 */
export async function loadEventLog(tailLines = 200): Promise<EventLogView> {
  return readEventLog(tailLines);
}
