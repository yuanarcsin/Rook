#!/usr/bin/env node
// 夹具：绑定指定 TCP 端口并保持监听，用于端口预检/冲突检测。
//
// 用法：
//   node port-listen.mjs <port> [hold-ms=60000]
//     port     监听端口（只绑 127.0.0.1，不对外暴露）
//     hold-ms  保持时长（毫秒）；到期关闭并退出 0
//
// 输出（UTF-8）：
//   "listening 127.0.0.1:<port>"  绑定成功后立即输出（测试据此等待）
//   "closed <port>"               到期关闭
//   stderr "bind failed code=<errno>"  端口被占用/权限不足
//
// 退出码：
//   到期 0；绑定失败 3；参数非法 2。
//
// 只监听本机回环地址，不连接任何外部服务。

import net from "node:net";

const port = Number.parseInt(process.argv[2] ?? "", 10);
const holdMs = Number.parseInt(process.argv[3] ?? "60000", 10) || 60000;

if (!Number.isInteger(port) || port <= 0 || port > 65535) {
  process.stderr.write("port-listen: invalid port\n");
  process.exitCode = 2;
} else {
  const server = net.createServer((socket) => {
    // 只接受连接后立即关闭：夹具不提供任何协议。
    socket.end();
  });

  server.on("error", (error) => {
    process.stderr.write(`bind failed code=${error.errno ?? "unknown"}\n`);
    process.exitCode = 3;
  });

  server.listen({ port, host: "127.0.0.1", exclusive: true }, () => {
    process.stdout.write(`listening 127.0.0.1:${port}\n`);
    setTimeout(() => {
      server.close(() => {
        process.stdout.write(`closed ${port}\n`);
        process.exitCode = 0;
      });
    }, holdMs);
  });
}
