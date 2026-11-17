#!/usr/bin/env node
// 夹具：按指定退出码退出，并在 stdout/stderr 写出可断言的标记。
//
// 用法：
//   node exit-code.mjs <code> [marker]
//     code    退出码（0-255）；0 = 正常退出，7 = 模拟崩溃
//     marker  可选标记文本（默认 "default"），可含 CJK/空格，用于断言参数传递
//
// 输出（UTF-8）：
//   stdout  "exit-code start marker=<marker>"
//           "exit-code done code=<code>"
//   stderr  "exit-code stderr marker=<marker>"
//
// 退出码：<code>（设置 process.exitCode 让 stdout 自然刷完，避免管道截断）
//
// 不访问网络、不读写除 stdout/stderr 之外的任何资源。

const code = Number.parseInt(process.argv[2] ?? "0", 10);
const marker = process.argv[3] ?? "default";

process.stdout.write(`exit-code start marker=${marker}\n`);
process.stderr.write(`exit-code stderr marker=${marker}\n`);
process.stdout.write(`exit-code done code=${code}\n`);

process.exitCode = Number.isInteger(code) ? code : 1;
