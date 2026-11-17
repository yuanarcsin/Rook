#!/usr/bin/env node
// 夹具：快速产生大量标准输出，用于日志落盘/轮转/背压验证。
//
// 用法：
//   node bulk-output.mjs [lines=2000] [pad-bytes=64]
//     lines     输出行数
//     pad-bytes 每行填充的 ASCII 字节数（总输出约 lines * (pad-bytes + 16)）
//
// 输出（UTF-8）：
//   "bulk start lines=<lines> pad=<pad>"
//   每行 "bulk <序号> <填充>"
//   最后 "bulk done lines=<lines> bytes=<写入字节数>"
//
// 退出码：0（写满即正常退出；stdout 背压时等待 drain，不丢字节）
//
// 说明：输出总量刻意可控（默认约 160KB），配合测试里很小的 max_file_bytes
// 就能触发多次 size 轮转，而不需要生成 100MB 大文件。

const lines = Number.parseInt(process.argv[2] ?? "2000", 10) || 2000;
const padBytes = Number.parseInt(process.argv[3] ?? "64", 10) || 64;
const pad = "x".repeat(padBytes);

let written = 0;

/** 写一行；返回的 Promise 在需要时等待管道 drain（背压）。 */
function writeLine(text) {
  written += Buffer.byteLength(text);
  if (process.stdout.write(text)) {
    return Promise.resolve();
  }
  return new Promise((resolve) => process.stdout.once("drain", resolve));
}

await writeLine(`bulk start lines=${lines} pad=${padBytes}\n`);
for (let i = 0; i < lines; i += 1) {
  await writeLine(`bulk ${i} ${pad}\n`);
}
await writeLine(`bulk done lines=${lines} bytes=${written}\n`);
