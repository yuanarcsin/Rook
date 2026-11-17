#!/usr/bin/env node
// 夹具：Node 父进程派生孙进程后**长驻**，用于 stop/drop 回收整棵进程树。
//
// 用法：
//   node spawn-child.mjs <pidfile> [hold-ms=60000]
//     pidfile  孙进程把自己的 PID 写到这里（测试按 PID 检查，不按进程名）
//     hold-ms  父子进程共同的保持时长（毫秒）
//
// 输出（UTF-8）：
//   "parent ready pid=<pid>"
//   "grandchild spawned pid=<pid>"
//   "parent holding"
//   "parent done"（到期）
//   （孙进程）"grandchild ready pid=<pid>" / "grandchild done pid=<pid>"
//
// 退出码：到期 0；被 TerminateJobObject 终止时为 1。
//
// 说明：Node（libuv）为每个非 detached 子进程建立带 KILL_ON_JOB_CLOSE 的 Job，
// 父进程退出会连带杀死子进程，因此"父进程退出、孙进程仍活"的场景由
// `spawn-child.cmd`（cmd 启动器）覆盖，而不是本脚本。

import { spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const pidFile = process.argv[2];
const holdMs = Number.parseInt(process.argv[3] ?? "60000", 10) || 60000;

if (!pidFile) {
  process.stderr.write("spawn-child: missing <pidfile>\n");
  process.exitCode = 2;
} else {
  const scriptDir = path.dirname(fileURLToPath(import.meta.url));

  process.stdout.write(`parent ready pid=${process.pid}\n`);

  // stdio 继承：孙进程继续持有同一组管道，直到整棵树被回收。
  const child = spawn(
    process.execPath,
    [path.join(scriptDir, "grandchild.mjs"), pidFile, String(holdMs)],
    { stdio: ["ignore", "inherit", "inherit"], windowsHide: true },
  );
  child.on("error", (error) => {
    process.stderr.write(`spawn grandchild failed: ${error.message}\n`);
    process.exitCode = 4;
  });
  process.stdout.write(`grandchild spawned pid=${child.pid}\n`);
  process.stdout.write("parent holding\n");

  setTimeout(() => {
    process.stdout.write("parent done\n");
    process.exitCode = 0;
  }, holdMs);
}
