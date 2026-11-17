// 日志轮询的纯判定（Svelte/DOM 无关，便于单测）。
//
// 组件只保留定时器、状态赋值与滚动副作用；是否继续轮询、响应是否过期、
// 是否自动滚动都在这里判定，节奏与 LogsTab 原有实现保持一致。

/** 与后端默认值一致：默认最近 200 行、最多 256KB。 */
export const LOG_POLL_MS = 2000;

/** 行数下拉选项。 */
export const TAIL_OPTIONS = [100, 200, 500, 1000];

/** 新的请求序号；异步响应回来时用它丢弃过期结果。 */
export function nextRequestSeq(current: number): number {
  return current + 1;
}

/** 响应/错误是否已被更新的请求取代。 */
export function isStaleResponse(seq: number, current: number): boolean {
  return seq !== current;
}

/** 只有最新一次非静默请求才结束后台的读取中状态。 */
export function shouldClearLoading(
  seq: number,
  current: number,
  silent: boolean,
): boolean {
  return seq === current && !silent;
}

/** 是否应启动轮询：自动刷新开启且选中了任务。 */
export function shouldPoll(autoRefresh: boolean, taskId: string | null): boolean {
  return autoRefresh && taskId !== null;
}

/** 是否应自动滚到底部：跟随开启且已有日志内容。 */
export function shouldAutoScroll(follow: boolean, hasView: boolean): boolean {
  return follow && hasView;
}

/**
 * 切换日志文件（C3 归档浏览）时是否需要重置跟随：
 * 只看文件标识是否变化，任务首屏（null → 首个文件）不算切换。
 */
export function shouldResetFollow(
  prevFileId: string | null,
  nextFileId: string | null,
): boolean {
  return prevFileId !== null && prevFileId !== nextFileId;
}

/** 搜索过滤：空查询返回原数组，大小写不敏感。 */
export function filterLogLines(lines: string[], query: string): string[] {
  const needle = query.trim().toLowerCase();
  if (needle.length === 0) {
    return lines;
  }
  return lines.filter((line) => line.toLowerCase().includes(needle));
}
