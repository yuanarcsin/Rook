// 详情卡片的纯格式化函数（M4-6）。
//
// 不依赖 Svelte / DOM：只把后端给的 epoch 毫秒与 id 转成展示文本，
// 缺失字段统一返回「—」，调用方无需再做 null 判断。

/** 缺失字段的占位符。 */
export const MISSING = "—";

/** instanceId 短显示保留的字符数。 */
const SHORT_ID_LEN = 8;

function isEpochMs(value: number | null | undefined): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

/** epoch 毫秒 → 本地时间文本；`null` / 非法值返回「—」。 */
export function formatTimestamp(ms: number | null | undefined): string {
  return isEpochMs(ms) ? new Date(ms).toLocaleString() : MISSING;
}

/** 长 id（如 instanceId）短显示：保留前 8 位，其余用省略号。 */
export function shortId(id: string | null | undefined): string {
  if (typeof id !== "string" || id.length === 0) {
    return MISSING;
  }
  return id.length <= SHORT_ID_LEN ? id : `${id.slice(0, SHORT_ID_LEN)}…`;
}

/**
 * 距 `targetMs` 的剩余时间：已到点显示「已到点」，否则 `mm:ss`
 * （超过 1 小时为 `h:mm:ss`）。`targetMs` 缺失返回「—」。
 */
export function formatCountdown(
  targetMs: number | null | undefined,
  nowMs: number,
): string {
  if (!isEpochMs(targetMs) || !isEpochMs(nowMs)) {
    return MISSING;
  }
  const remaining = targetMs - nowMs;
  if (remaining <= 0) {
    return "已到点";
  }
  const totalSeconds = Math.ceil(remaining / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  const pad = (value: number) => String(value).padStart(2, "0");
  return hours > 0
    ? `${hours}:${pad(minutes)}:${pad(seconds)}`
    : `${pad(minutes)}:${pad(seconds)}`;
}

/** 本地时间文本（今天/明天/月日 + 时:分），用于“下次自动启动”这类近期时刻。 */
export function formatWhen(ms: number | null | undefined, nowMs: number = Date.now()): string {
  if (!isEpochMs(ms) || !isEpochMs(nowMs)) {
    return MISSING;
  }
  const date = new Date(ms);
  const now = new Date(nowMs);
  const pad = (value: number) => String(value).padStart(2, "0");
  const time = pad(date.getHours()) + ":" + pad(date.getMinutes());
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
  const target = new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
  const days = Math.round((target - today) / 86_400_000);
  if (days === 0) {
    return "今天 " + time;
  }
  if (days === 1) {
    return "明天 " + time;
  }
  if (days === -1) {
    return "昨天 " + time;
  }
  return date.getMonth() + 1 + " 月 " + date.getDate() + " 日 " + time;
}

/** 已运行时长：`12s` / `3m` / `2h 13m` / `1d 4h`；缺失返回「—」。 */
export function formatElapsed(
  sinceMs: number | null | undefined,
  nowMs: number,
): string {
  if (!isEpochMs(sinceMs) || !isEpochMs(nowMs)) {
    return MISSING;
  }
  const totalSeconds = Math.max(0, Math.floor((nowMs - sinceMs) / 1000));
  if (totalSeconds < 60) {
    return `${totalSeconds}s`;
  }
  const totalMinutes = Math.floor(totalSeconds / 60);
  if (totalMinutes < 60) {
    return `${totalMinutes}m`;
  }
  const hours = Math.floor(totalMinutes / 60);
  if (hours < 24) {
    const minutes = totalMinutes % 60;
    return minutes > 0 ? `${hours}h ${minutes}m` : `${hours}h`;
  }
  return `${Math.floor(hours / 24)}d ${hours % 24}h`;
}
