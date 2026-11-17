#!/usr/bin/env node
// 夹具：模拟"不配合优雅停止"的脚本（收到 SIGBREAK 但拒绝退出）。
//
// 用法：
//   node graceful-ignore.mjs <marker>
//
// 文件协议：
//   <marker>.ready   注册完 SIGBREAK handler 后立即写
//   <marker>.signal  收到 SIGBREAK 后写（证明信号已送达），然后继续运行
//
// 行为：永不主动退出，只能被 TerminateJobObject 强制结束（退出码 1）。

import fs from "node:fs";

const marker = process.argv[2];

if (!marker) {
  process.stderr.write("graceful-ignore: missing <marker>\n");
  process.exitCode = 2;
} else {
  process.on("SIGBREAK", () => {
    // 故意不退出：用于验证宽限期到期后的 Job 兜底。
    fs.writeFileSync(`${marker}.signal`, `signal pid=${process.pid}\n`);
  });

  fs.writeFileSync(`${marker}.ready`, `${process.pid}\n`);

  setInterval(() => {}, 1000);
}
