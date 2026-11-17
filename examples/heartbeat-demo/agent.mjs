// 最小可用的「被 Rook 监督的脚本」示例。
//
// 职责只有两件事：
//   1. 正常干活（这里用每 2 秒一次 tick 代替）；
//   2. 把心跳写到 CAMPUS_SENTRY_HEARTBEAT 指向的文件里。
//
// 心跳协议见 README「健康探测」一节：
//   - 一行 JSON；instance/seq/ts 用于确认「确实在写新内容」；
//   - status 用 ok/degraded/blocked/login_required 表达业务状态；
//   - 原子替换（tmp + rename），避免核心读到半截 JSON。
//
// 直接运行：node agent.mjs
// 被 Rook 启动时，CAMPUS_SENTRY_HEARTBEAT 由应用自动注入，无需配置。

import { renameSync, writeFileSync } from "node:fs";

const heartbeatPath = process.env.CAMPUS_SENTRY_HEARTBEAT;
const instance = process.env.CAMPUS_SENTRY_INSTANCE_ID || "dev";
let seq = 0;

/** 原子写一行心跳；没有心跳路径时静默跳过（便于本地直接运行）。 */
function beat(status = "ok", detail = "") {
  if (!heartbeatPath) return;
  const payload = JSON.stringify({
    instance,
    seq: seq++,
    ts: new Date().toISOString(),
    status,
    detail,
  });
  const tmp = `${heartbeatPath}.tmp`;
  writeFileSync(tmp, `${payload}\n`, "utf8");
  renameSync(tmp, heartbeatPath);
}

beat("ok", "starting");
console.log(`[demo] started; heartbeat -> ${heartbeatPath ?? "(not configured)"}`);

const timer = setInterval(() => {
  // 真实脚本在这里做一轮业务工作，并把结果映射成 status。
  console.log(`[demo] tick ${new Date().toISOString()}`);
  beat("ok", "alive");
}, 2000);

/** Rook 的优雅停止会向进程组发 CTRL_BREAK（Node 里是 SIGBREAK）。 */
function shutdown(signal) {
  clearInterval(timer);
  beat("stopped", "shutdown");
  console.log(`[demo] ${signal}, bye`);
  process.exit(0);
}

process.on("SIGBREAK", () => shutdown("SIGBREAK"));
process.on("SIGINT", () => shutdown("SIGINT"));
process.on("SIGTERM", () => shutdown("SIGTERM"));
