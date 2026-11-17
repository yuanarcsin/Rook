#!/usr/bin/env node
// 夹具：把收到的参数、工作目录与环境变量原样写到 stdout，验证引号/编码/工作目录。
//
// 用法：
//   node echo-args.mjs [arg...]
//     任意参数（可含 CJK、空格、制表符、引号、反斜杠、空串、结尾空格）
//
// 输出（UTF-8，每行一条，便于逐行断言）：
//   "argc=<参数个数>"
//   "argv=<序号>:<参数原文>"    每个参数一行（序号从 0 开始）
//   "cwd=<当前工作目录>"
//   "env:<变量名>=<值>"         仅当变量存在（测试用 CS_FIXTURE_MARK）
//
// 退出码：0（始终正常退出，便于测试区分“参数丢失”与“启动失败”）
//
// 参数原样输出，不做 trim/转义；测试直接按字节比对，能发现引号处理缺陷。

const args = process.argv.slice(2);

process.stdout.write(`argc=${args.length}\n`);
for (let i = 0; i < args.length; i += 1) {
  process.stdout.write(`argv=${i}:${args[i]}\n`);
}
process.stdout.write(`cwd=${process.cwd()}\n`);

for (const name of ["CS_FIXTURE_MARK"]) {
  if (process.env[name] !== undefined) {
    process.stdout.write(`env:${name}=${process.env[name]}\n`);
  }
}
