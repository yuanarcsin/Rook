#!/usr/bin/env node
// 夹具：模拟"配合优雅停止"的脚本。
//
// 用法：
//   node graceful-exit.mjs <marker>
//     marker  标记文件路径（绝对或相对）
//
// 文件协议：
//   <marker>.ready   注册完 SIGBREAK handler 后立即写（测试先等它再发信号，
//                    避免在 handler 就绪前被默认处理程序打死，见 spike §4.1 #10）
//   <marker>         收到 SIGBREAK 后写，内容含 pid
//
// 行为：收到 SIGBREAK 后约 200ms 以 0 退出；未收到信号则一直运行。
// 不访问网络、不写除标记文件外的任何资源。

import fs from "node:fs";

const marker = process.argv[2];

if (!marker) {
  process.stderr.write("graceful-exit: missing <marker>\n");
  process.exitCode = 2;
} else {
  let received = false;

  process.on("SIGBREAK", () => {
    if (received) {
      return;
    }
    received = true;
    fs.writeFileSync(marker, `signal pid=${process.pid}\n`);
    // 给清理留一点时间，模拟真实脚本落盘/关连接。
    setTimeout(() => process.exit(0), 200);
  });

  fs.writeFileSync(`${marker}.ready`, `${process.pid}\n`);

  // 保活：等待控制台事件。
  setInterval(() => {}, 1000);
}
