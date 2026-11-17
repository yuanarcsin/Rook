// 应用内通知的「人话」层：把后端通知翻译成普通用户看得懂的标题/说明，
// 变量名、原始错误、任务 id 一律留给详情面板。
//
// 后端 message 面向日志与系统 Toast，允许出现 exit_code、ZHS_UUID 这类字眼；
// 应用内主视图不直接展示它，避免一屏都是开发者字段（docs/05 §7.3、§8）。

import type { NotificationKind, NotificationPayload } from "$lib/api";

/** 通知的视觉语气：bad（需要处理）/ warn（等待人工）/ info（一般提示）。 */
export type NotificationTone = "bad" | "warn" | "info";

/** 通知类型的中文名，详情面板里作为「类型」一行展示。 */
export const notificationKindLabel: Record<NotificationKind, string> = {
  needs_human: "等待人工处理",
  crash_loop: "脚本已暂停",
  restart_budget: "脚本已暂停（重启过频）",
  start_failed: "启动失败",
  stop_timeout: "停止超时",
  config_error: "配置有误",
  config_warning: "配置提醒",
};

export type HumanNotification = {
  /** 一句话说清发生了什么。 */
  title: string;
  /** 影响范围 + 下一步；不出现字段名/退出码/任务 id。 */
  summary: string;
  tone: NotificationTone;
};

/**
 * 把通知转成主视图文案。
 *
 * 约定：
 * - 标题给「谁出了什么事」，摘要给「影响 + 该怎么办」；
 * - 不拼接后端 message（其中可能有变量名与原始错误），原文只在详情里出现；
 * - 多个任务合并的通知用「N 个任务」概括，不逐个列 id。
 */
export function humanizeNotification(
  notification: NotificationPayload,
): HumanNotification {
  const name = notification.taskName?.trim() || null;
  const who = name === null ? "有脚本" : `「${name}」`;

  if (notification.kind === "config_warning") {
    return {
      title:
        notification.count > 1 ? `${notification.count} 条配置提醒` : "有一条配置提醒",
      summary: "不影响正在运行的脚本；方便时改一下就好，展开能看到具体位置。",
      tone: "info",
    };
  }

  if (notification.count > 1) {
    return {
      title: `${notification.count} 个脚本需要你处理`,
      summary: "它们同时报了问题。逐条看一下就行，处理好后会自动恢复监督。",
      tone: "bad",
    };
  }

  switch (notification.kind) {
    case "needs_human":
      return {
        title: `${who}在等你处理`,
        summary:
          "脚本正等待扫码或登录。处理好之后会自动继续；在这之前不会反复重启。",
        tone: "warn",
      };
    case "crash_loop":
      return {
        title: `${who}反复失败，已暂停`,
        summary:
          "自动重启已暂时关闭，避免一直失败。查看日志修好后，可以在脚本卡上手动启动。",
        tone: "bad",
      };
    case "restart_budget":
      return {
        title: `${who}重启过于频繁，已暂停`,
        summary:
          "最近一段时间启动次数超过上限，自动重启已关闭。先看日志确认崩溃原因，修好后手动启动。",
        tone: "bad",
      };
    case "start_failed":
      return {
        title: `${who}没能启动`,
        summary:
          "进程没有跑起来，通常是命令、工作目录或端口有问题。确认后可以手动重试。",
        tone: "bad",
      };
    case "stop_timeout":
      return {
        title: `${who}可能还在运行`,
        summary:
          "停止请求已经发出，但进程没有按时退出，可能仍有残留进程占用端口。",
        tone: "warn",
      };
    case "config_error":
      return {
        title: "配置文件有一处写错了",
        summary: "这次修改没有生效；正在运行的脚本不受影响，修好后重新加载即可。",
        tone: "bad",
      };
  }
}

/** 相对时间：刚刚 / N 分钟前 / N 小时前 / 月日 时:分。 */
export function formatRelativeTime(at: number, now = Date.now()): string {
  if (!Number.isFinite(at)) {
    return "—";
  }
  const diff = Math.max(0, now - at);
  if (diff < 60_000) {
    return "刚刚";
  }
  if (diff < 3_600_000) {
    return `${Math.floor(diff / 60_000)} 分钟前`;
  }
  if (diff < 86_400_000) {
    return `${Math.floor(diff / 3_600_000)} 小时前`;
  }
  const date = new Date(at);
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getMonth() + 1} 月 ${date.getDate()} 日 ${pad(date.getHours())}:${pad(
    date.getMinutes(),
  )}`;
}
