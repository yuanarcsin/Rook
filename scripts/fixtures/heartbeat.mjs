#!/usr/bin/env node
// 夹具：周期性原子写心跳文件（JSON），用于心跳新鲜度/进程存活观察。
//
// 用法：
//   node heartbeat.mjs <heartbeat-file> [interval-ms=200] [count=0]
//     heartbeat-file  心跳文件路径（可含 CJK/空格；父目录需已存在）
//     interval-ms     写入间隔（毫秒）
//     count           写入次数；0 表示不限（长驻，直到被停止）
//
// 心跳内容（UTF-8 JSON，先写临时文件再 rename，读者不会看到半截内容）：
//   {"pid":<pid>,"seq":<序号>,"status":"ok","ts":"<ISO-8601>"}
//
// 输出（UTF-8）：
//   "heartbeat ready pid=<pid> file=<path>"
//   每次写入后 "heartbeat seq=<序号>"
//   "heartbeat done count=<次数>"（有限次数结束时）
//
// 退出码：写满 count 后 0；不限次数时被停止（TerminateJobObject）为 1。
//
// 不访问网络；只写指定文件与同目录的临时文件。

import fs from "node:fs";

const file = process.argv[2];
const intervalMs = Number.parseInt(process.argv[3] ?? "200", 10) || 200;
const count = Number.parseInt(process.argv[4] ?? "0", 10) || 0;

if (!file) {
  process.stderr.write("heartbeat: missing <heartbeat-file>\n");
  process.exitCode = 2;
} else {
  const tmp = `${file}.tmp`;
  let seq = 0;

  const writeBeat = () => {
    seq += 1;
    const beat = {
      pid: process.pid,
      seq,
      status: "ok",
      ts: new Date().toISOString(),
    };
    fs.writeFileSync(tmp, `${JSON.stringify(beat)}\n`);
    fs.renameSync(tmp, file);
    process.stdout.write(`heartbeat seq=${seq}\n`);
  };

  process.stdout.write(`heartbeat ready pid=${process.pid} file=${file}\n`);
  writeBeat();

  const timer = setInterval(() => {
    writeBeat();
    if (count > 0 && seq >= count) {
      clearInterval(timer);
      process.stdout.write(`heartbeat done count=${seq}\n`);
      process.exitCode = 0;
    }
  }, intervalMs);
}
