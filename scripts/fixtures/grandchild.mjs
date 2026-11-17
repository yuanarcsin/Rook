#!/usr/bin/env node
// 夹具：长驻孙进程，写自己的 PID 后一直运行，用于进程树/孤儿检测。
//
// 用法：
//   node grandchild.mjs <pidfile> [hold-ms=60000]
//     pidfile  写十进制 PID + 换行（测试按 PID 检查存活，绝不按进程名）
//     hold-ms  保持时长（毫秒）；到期自行退出 0
//
// 输出（UTF-8）：
//   "grandchild ready pid=<pid>"
//   "grandchild done pid=<pid>"（到期）
//
// 退出码：到期 0；被 TerminateJobObject 终止时为 1。
//
// 本脚本不自己派生进程，由父进程（Node 或 cmd 启动器）拉起；
// stdout/stderr 继承父进程的管道，因此父进程退出后仍能继续写日志。

import fs from "node:fs";

const pidFile = process.argv[2];
const holdMs = Number.parseInt(process.argv[3] ?? "60000", 10) || 60000;

if (!pidFile) {
  process.stderr.write("grandchild: missing <pidfile>\n");
  process.exitCode = 2;
} else {
  fs.writeFileSync(pidFile, `${process.pid}\n`);
  process.stdout.write(`grandchild ready pid=${process.pid}\n`);
  setTimeout(() => {
    process.stdout.write(`grandchild done pid=${process.pid}\n`);
    process.exitCode = 0;
  }, holdMs);
}
