// 产品显示名（前端唯一来源）：Rook。
//
// 磁盘/安装身份也是 Rook（exe、identifier、配置/数据目录、自启 Run 值、
// 插件清单名、日志名）；旧名 CampusSentry 只在启动迁移与兼容读取里出现。
// 改名字时同步：src-tauri/src/brand.rs、src/app.html、tauri.conf.json 与文档标题。
export const BRAND = {
  name: "Rook",
  tagline: "本机脚本的常驻值守",
  promise: "让本机脚本一直在岗",
} as const;
