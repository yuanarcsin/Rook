#!/usr/bin/env node
// 夹具：低 CPU 长驻进程，用于 start → running → stop 生命周期与强制终止。
//
// 用法：
//   node long-run.mjs [hold-ms=60000] [marker]
//     hold-ms  保持时长（毫秒）；到期后自行退出 0
//     marker   可选标记，写进首行，便于断言
//
// 输出（UTF-8）：
//   "long-run ready pid=<pid> marker=<marker>"
//   每 200ms 一行 "long-run tick n=<序号>"（验证停止前日志不丢）
//   到期时 "long-run done ticks=<总数>"
//
// 退出码：
//   到期自然退出 0；
//   被 TerminateJobObject 强制终止时为 1（停止路径的预期结果）。

const holdMs = Number.parseInt(process.argv[2] ?? "60000", 10);
const marker = process.argv[3] ?? "default";

process.stdout.write(`long-run ready pid=${process.pid} marker=${marker}\n`);

let ticks = 0;
const timer = setInterval(() => {
  ticks += 1;
  process.stdout.write(`long-run tick n=${ticks}\n`);
}, 200);

setTimeout(() => {
  clearInterval(timer);
  process.stdout.write(`long-run done ticks=${ticks}\n`);
  process.exitCode = 0;
}, Number.isInteger(holdMs) && holdMs > 0 ? holdMs : 60000);
