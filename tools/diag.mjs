#!/usr/bin/env node
// Rook 诊断/日志查询工具（2026-11-04）。
//
// 零依赖，供 agent 与用户直接查询真实场景日志：
//   node tools/diag.mjs status
//   node tools/diag.mjs events --tail 200 --task ykt-gateway
//   node tools/diag.mjs query --task ykt-gateway --grep checkin
//   node tools/diag.mjs collect --out D:/tmp/cs-diag
//
// 默认数据目录：生产 LOCALAPPDATA/Rook（真实测试用）；
// dev 用 --dev 或 --data-dir src-tauri/.dev-data；--data-dir 优先级最高。

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function parseArgs(argv) {
  const out = { _: [] };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg.startsWith("--")) {
      const key = arg.slice(2);
      const next = argv[i + 1];
      if (next && !next.startsWith("--")) {
        out[key] = next;
        i += 1;
      } else {
        out[key] = true;
      }
    } else {
      out._.push(arg);
    }
  }
  return out;
}

function defaultDataDir() {
  const local = process.env.LOCALAPPDATA;
  if (local) return path.join(local, "Rook");
  return path.join(os.homedir(), "AppData", "Local", "Rook");
}

function configuredDataDir(configFile) {
  try {
    const text = fs.readFileSync(configFile, "utf8");
    const match = text.match(/^\s*data_dir\s*=\s*["']([^"']+)["']/m);
    if (!match) return null;
    const expanded = match[1].replace(/%(\w+)%/g, (_, name) => process.env[name] ?? "");
    if (!expanded) return null;
    return path.isAbsolute(expanded)
      ? expanded
      : path.resolve(path.dirname(configFile), expanded);
  } catch {
    return null;
  }
}

function defaultConfigPath() {
  const candidates = [];
  if (process.env.APPDATA) {
    candidates.push(path.join(process.env.APPDATA, "Rook", "config.toml"));
  }
  return candidates.find((file) => fs.existsSync(file)) ?? null;
}

function dataDir(args) {
  if (args["data-dir"]) return path.resolve(args["data-dir"]);
  if (args.dev) return path.join(root, "src-tauri", ".dev-data");
  const config = defaultConfigPath();
  const configured = config ? configuredDataDir(config) : null;
  return configured ?? defaultDataDir();
}

function findConfig(dir) {
  const candidates = [
    path.join(dir, "Rook.toml"),
    path.join(dir, "..", "Rook.toml"),
  ];
  if (process.env.APPDATA) {
    candidates.push(path.join(process.env.APPDATA, "Rook", "config.toml"));
  }
  return candidates.find((file) => fs.existsSync(file)) || null;
}

function readJson(file) {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8"));
  } catch {
    return null;
  }
}

function listFiles(dir, filter = () => true) {
  const out = [];
  if (!fs.existsSync(dir)) return out;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...listFiles(full, filter));
    else if (filter(full)) out.push(full);
  }
  return out;
}

function tailLines(file, count) {
  try {
    const text = fs.readFileSync(file, "utf8");
    const lines = text.split(/\r?\n/);
    if (lines.length && lines[lines.length - 1] === "") lines.pop();
    return lines.slice(-count);
  } catch {
    return [];
  }
}

function eventFiles(dir) {
  const core = path.join(dir, "core");
  return listFiles(core, (file) => path.basename(file).startsWith("events.jsonl"));
}

function readEvents(dir, { tail = 200, task, level, grep } = {}) {
  const lines = [];
  for (const file of eventFiles(dir)) {
    for (const line of tailLines(file, tail)) {
      if (!line.trim()) continue;
      let item = null;
      try { item = JSON.parse(line); } catch { continue; }
      if (task && item.fields?.task !== task && item.task !== task) continue;
      if (level && String(item.level).toLowerCase() !== level.toLowerCase()) continue;
      if (grep && !line.toLowerCase().includes(grep.toLowerCase())) continue;
      lines.push(item);
    }
  }
  return lines.slice(-tail);
}

function stateSummary(dir) {
  const state = readJson(path.join(dir, "state.json")) || {};
  const tasks = Object.entries(state.tasks || {}).map(([id, value]) => ({
    id,
    status: value.last_status || null,
    override: value.desired_override || null,
    paused: value.paused_reason || null,
    needsHuman: value.needs_human?.active || false,
    lastExitCode: value.last_exit_code ?? null,
    starts: value.total_starts ?? 0,
    restarts: value.total_restarts ?? 0,
    scheduleWindow: value.schedule_window || null,
    scheduleSuppressed: value.schedule_suppressed || null,
  }));
  const logs = {};
  const logRoot = path.join(dir, "logs");
  if (fs.existsSync(logRoot)) {
    for (const entry of fs.readdirSync(logRoot, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const current = path.join(logRoot, entry.name, "current.log");
      const stat = fs.existsSync(current) ? fs.statSync(current) : null;
      logs[entry.name] = stat ? { bytes: stat.size, modifiedAt: stat.mtime.toISOString() } : null;
    }
  }
  return {
    dataDir: dir,
    configPath: findConfig(dir),
    stateTasks: tasks,
    logs,
    eventFiles: eventFiles(dir).map((file) => ({ file, bytes: fs.statSync(file).size })),
  };
}

function collect(dir, out) {
  const target = path.resolve(out || path.join(dir, "diagnostics", `diag-${Date.now()}`));
  fs.mkdirSync(target, { recursive: true });
  let files = 0;
  const copy = (source, destination) => {
    if (!fs.existsSync(source) || !fs.statSync(source).isFile()) return;
    fs.mkdirSync(path.dirname(destination), { recursive: true });
    fs.copyFileSync(source, destination);
    files += 1;
  };
  const configPath = findConfig(dir);
  if (configPath) {
    copy(configPath, path.join(target, "config.toml"));
  }
  copy(path.join(dir, "state.json"), path.join(target, "state.json"));
  for (const file of listFiles(path.join(dir, "core"))) {
    copy(file, path.join(target, "core", path.basename(file)));
  }
  for (const file of listFiles(path.join(dir, "logs"))) {
    const relative = path.relative(path.join(dir, "logs"), file);
    copy(file, path.join(target, "logs", relative));
  }
  for (const file of listFiles(path.join(dir, "heartbeats"))) {
    copy(file, path.join(target, "heartbeats", path.basename(file)));
  }
  const summary = [
    "# Rook 诊断包",
    "",
    `- 数据目录：${dir}`,
    `- 导出时间：${new Date().toISOString()}`,
    `- 文件数：${files}`,
    "",
    "## 任务状态",
    "```json",
    JSON.stringify(stateSummary(dir).stateTasks, null, 2),
    "```",
    "",
    "## 查询建议",
    "```powershell",
    "node tools/diag.mjs events --tail 200",
    "Get-Content core/events.jsonl -Tail 200",
    "Get-Content core/rook.log -Tail 200",
    "```",
  ].join("\n");
  fs.writeFileSync(path.join(target, "summary.md"), summary, "utf8");
  return { path: target, files: files + 1 };
}

function formatEntry(item) {
  const fields = item.fields || {};
  const task = fields.task ? ` [${fields.task}]` : "";
  const message = item.message || "";
  return `${item.ts} ${String(item.level).toUpperCase()}${task} ${message}`;
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const command = args._[0] || "status";
  const dir = dataDir(args);
  if (!fs.existsSync(dir)) {
    console.error(`数据目录不存在：${dir}`);
    process.exit(2);
  }
  if (command === "status") {
    console.log(JSON.stringify(stateSummary(dir), null, 2));
    return;
  }
  if (command === "events") {
    const events = readEvents(dir, {
      tail: Number(args.tail || 200),
      task: args.task,
      level: args.level,
      grep: args.grep,
    });
    for (const item of events) console.log(formatEntry(item));
    return;
  }
  if (command === "query") {
    const task = args.task;
    if (!task) {
      console.error("query 需要 --task <id>");
      process.exit(2);
    }
    const log = path.join(dir, "logs", task, "current.log");
    const lines = tailLines(log, Number(args.tail || 300));
    const grep = args.grep ? String(args.grep).toLowerCase() : null;
    for (const line of lines) {
      if (!grep || line.toLowerCase().includes(grep)) console.log(line);
    }
    return;
  }
  if (command === "collect") {
    const result = collect(dir, args.out);
    console.log(JSON.stringify(result, null, 2));
    return;
  }
  console.error(`未知命令：${command}（可用：status/events/query/collect）`);
  process.exit(2);
}

main();